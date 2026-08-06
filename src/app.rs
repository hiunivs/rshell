use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use crate::config::{self, AppConfig};
use crate::file_manager::FileManager;
use crate::install_app_fonts;
use crate::session::monitor::{LocalSampler, ProcInfo, SystemMetrics};
use crate::session::ssh::SshParams;
use crate::term_view::{TermDims, TermView};

fn no_file_manager_debug() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| std::env::var_os("RSHELL_NO_FM").is_some())
}

fn no_terminal_debug() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| std::env::var_os("RSHELL_NO_TERM").is_some())
}

/// 会话类型;SysInfo(源会话 id) 是挂在某个会话下的系统信息页标签
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// 保存连接选择页；不创建终端或后台连接
    Launcher,
    Local,
    Ssh,
    SysInfo(usize),
}

/// 侧栏监控视图状态(每个会话一份)
#[derive(Clone, Default)]
pub struct MonitorView {
    pub metrics: Option<Arc<SystemMetrics>>,
    /// 每次采样时计算一次，UI 每帧只克隆 Arc。
    pub top_mem: Arc<Vec<ProcInfo>>,
    pub top_cpu: Arc<Vec<ProcInfo>>,
    /// (rx_bps, tx_bps) 历史,最多 120 点(聚合,不含 lo)
    pub net_history: VecDeque<(f64, f64)>,
    /// 分网卡速率历史(网卡名 → (rx_bps, tx_bps),最多 120 点)
    pub net_histories: std::collections::HashMap<String, VecDeque<(f64, f64)>>,
    /// 网速卡片选中的网卡;None = 全部(聚合)
    pub net_sel: Option<String>,
    pub cpu_history: VecDeque<f32>,
}

/// 侧栏进程列表的排序方式(点击"内存"/"CPU"表头切换)
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcSort {
    #[default]
    Mem,
    Cpu,
}

impl MonitorView {
    pub fn update(&mut self, m: Arc<SystemMetrics>) {
        self.net_history.push_back((m.net_rx_bps, m.net_tx_bps));
        while self.net_history.len() > 120 {
            self.net_history.pop_front();
        }
        // 分网卡历史;网卡消失(拔卡/断开)时清掉对应历史与选中态
        for n in &m.nets {
            let h = self.net_histories.entry(n.name.clone()).or_default();
            h.push_back((n.rx_bps, n.tx_bps));
            while h.len() > 120 {
                h.pop_front();
            }
        }
        self.net_histories
            .retain(|name, _| m.nets.iter().any(|n| &n.name == name));
        if let Some(sel) = &self.net_sel
            && !m.nets.iter().any(|n| &n.name == sel)
        {
            self.net_sel = None;
        }
        self.cpu_history.push_back(m.cpu_percent);
        while self.cpu_history.len() > 120 {
            self.cpu_history.pop_front();
        }
        self.top_mem = Arc::new(top_procs_owned(&m, ProcSort::Mem));
        self.top_cpu = Arc::new(top_procs_owned(&m, ProcSort::Cpu));
        self.metrics = Some(m);
    }
}

fn top_procs_owned(m: &SystemMetrics, sort: ProcSort) -> Vec<ProcInfo> {
    let mut procs: Vec<_> = m.procs.iter().collect();
    match sort {
        ProcSort::Mem => procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb)),
        ProcSort::Cpu => procs.sort_by(|a, b| {
            b.cpu
                .partial_cmp(&a.cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
    }
    procs.into_iter().take(10).cloned().collect()
}

/// 一个连接标签页(本地 shell 或 SSH 会话)
pub struct Session {
    /// 唯一 id(= 创建序号),SysInfo 标签用它引用源会话
    pub id: usize,
    pub title: String,
    pub kind: SessionKind,
    pub host: String,
    pub term: Option<TermView>,
    /// 终端启动失败时的错误信息
    pub error: Option<String>,
    pub monitor: MonitorView,
    pub fm: FileManager,
    /// 等待用户确认的主机指纹请求
    pub pending_hostkey: Option<crate::session::HostKeyPrompt>,
    /// SSH 连接参数(重连用;本机会话为 None)
    ssh_params: Option<SshParams>,
}

impl Session {
    /// SSH 保存连接启动页
    pub fn launcher(seq: usize) -> Self {
        Self {
            id: seq,
            title: "新建连接".into(),
            kind: SessionKind::Launcher,
            host: String::new(),
            term: None,
            error: None,
            monitor: MonitorView::default(),
            fm: FileManager::new_ssh(None),
            pending_hostkey: None,
            ssh_params: None,
        }
    }

    /// 本机 shell 会话
    pub fn local(ctx: &egui::Context, seq: usize) -> Self {
        match TermView::new(ctx, TermDims { cols: 80, rows: 24 }) {
            Ok((term, sftp_tx)) => Self {
                id: seq,
                title: format!("{seq} 本机"),
                kind: SessionKind::Local,
                host: "127.0.0.1".into(),
                term: Some(term),
                error: None,
                monitor: MonitorView::default(),
                fm: FileManager::new(Some(sftp_tx)),
                pending_hostkey: None,
                ssh_params: None,
            },
            Err(e) => Self {
                id: seq,
                title: format!("{seq} 本机"),
                kind: SessionKind::Local,
                host: "127.0.0.1".into(),
                term: None,
                error: Some(format!("启动本地 shell 失败:{e:#}")),
                monitor: MonitorView::default(),
                fm: FileManager::new(None),
                pending_hostkey: None,
                ssh_params: None,
            },
        }
    }

    /// SSH 会话(后台连接,进度显示在终端里)
    pub fn ssh(ctx: &egui::Context, seq: usize, params: SshParams) -> Self {
        let title = format!("{}@{}", params.user, params.host);
        let host = params.host.clone();
        let (term, sftp_tx) = TermView::ssh(ctx, TermDims { cols: 80, rows: 24 }, params.clone());
        Self {
            id: seq,
            title,
            kind: SessionKind::Ssh,
            host,
            term: Some(term),
            error: None,
            monitor: MonitorView::default(),
            fm: FileManager::new_ssh(Some(sftp_tx)),
            pending_hostkey: None,
            ssh_params: Some(params),
        }
    }

    /// 系统信息页标签:本身无终端,数据来自源会话(source_id)的监控
    pub fn sysinfo(seq: usize, source_id: usize, source_host: &str) -> Self {
        Self {
            id: seq,
            title: format!("{seq} 系统信息"),
            kind: SessionKind::SysInfo(source_id),
            host: source_host.to_string(),
            term: None,
            error: None,
            monitor: MonitorView::default(),
            fm: FileManager::new(None),
            pending_hostkey: None,
            ssh_params: None,
        }
    }

    /// 向终端写入输入
    pub fn send_str(&mut self, s: &str) {
        if let Some(term) = self.term.as_mut() {
            term.send_str(s);
        }
    }

    /// 是否处于连接状态(终端存在且未退出)
    pub fn is_connected(&self) -> bool {
        self.term.as_ref().is_some_and(|t| !t.is_exited())
    }

    /// SSH 连接参数(编辑窗口预填用;本机会话为 None)
    pub fn ssh_params(&self) -> Option<&SshParams> {
        self.ssh_params.as_ref()
    }

    /// 应用编辑后的连接参数：更新保存的参数、host 与标签标题。
    pub fn update_ssh_params(&mut self, params: SshParams) {
        let display_target = self
            .title
            .rsplit_once('@')
            .map(|(_, target)| target)
            .filter(|target| *target != self.host)
            .map(str::to_string);
        self.host = params.host.clone();
        self.title = format!(
            "{}@{}",
            params.user,
            display_target.as_deref().unwrap_or(&params.host)
        );
        self.ssh_params = Some(params);
    }

    /// 断开连接:通知会话线程关闭(连接断开/子进程被杀),
    /// 终端网格保留——旧输出仍可读,重连时在原内容上继续
    pub fn disconnect(&mut self) {
        if let Some(term) = self.term.as_mut() {
            term.disconnect();
        }
        self.fm = match self.kind {
            SessionKind::Local => FileManager::new(None),
            SessionKind::Launcher | SessionKind::Ssh | SessionKind::SysInfo(_) => {
                FileManager::new_ssh(None)
            }
        };
        self.monitor = MonitorView::default();
        self.pending_hostkey = None;
        self.error = None;
    }

    /// 重新连接:本机重开 shell,SSH 用保存的参数重连。
    /// 终端网格还在就只换通道(滚回内容保留);网格没了才新建
    pub fn reconnect(&mut self, ctx: &egui::Context) {
        self.pending_hostkey = None;
        self.monitor = MonitorView::default();
        match self.kind {
            // 启动页没有连接，不需要重连。
            SessionKind::Launcher => {}
            SessionKind::Local => {
                if let Some(term) = self.term.as_mut() {
                    let d = term.dims();
                    match crate::session::local::spawn(d.cols as u16, d.rows as u16, ctx.clone()) {
                        Ok(ch) => {
                            term.reconnect(
                                ch.cmd_tx,
                                ch.data_rx,
                                ch.event_rx,
                                ch.monitor_active_tx,
                            );
                            self.fm = FileManager::new(Some(ch.sftp_tx));
                            self.error = None;
                            return;
                        }
                        Err(e) => {
                            self.error = Some(format!("启动本地 shell 失败:{e:#}"));
                            return;
                        }
                    }
                }
                match TermView::new(ctx, TermDims { cols: 80, rows: 24 }) {
                    Ok((term, sftp_tx)) => {
                        self.term = Some(term);
                        self.fm = FileManager::new(Some(sftp_tx));
                        self.error = None;
                    }
                    Err(e) => {
                        self.term = None;
                        self.error = Some(format!("启动本地 shell 失败:{e:#}"));
                    }
                }
            }
            SessionKind::Ssh => {
                let Some(params) = self.ssh_params.clone() else {
                    self.error = Some("缺少连接参数,无法重连".into());
                    return;
                };
                if let Some(term) = self.term.as_mut() {
                    let d = term.dims();
                    let ch = crate::session::ssh::spawn(
                        params,
                        d.cols as u16,
                        d.rows as u16,
                        ctx.clone(),
                    );
                    term.reconnect(ch.cmd_tx, ch.data_rx, ch.event_rx, ch.monitor_active_tx);
                    self.fm = FileManager::new_ssh(Some(ch.sftp_tx));
                    self.error = None;
                } else {
                    let (term, sftp_tx) =
                        TermView::ssh(ctx, TermDims { cols: 80, rows: 24 }, params);
                    self.term = Some(term);
                    self.fm = FileManager::new_ssh(Some(sftp_tx));
                    self.error = None;
                }
            }
            // 系统信息页没有自己的连接,无重连语义
            SessionKind::SysInfo(_) => {}
        }
    }
}

/// 将最近创建且仍为空白的 SSH 启动页移动到标签末尾，并返回其新下标。
fn move_existing_launcher_to_end(sessions: &mut Vec<Session>) -> Option<usize> {
    let launcher_index = sessions
        .iter()
        .rposition(|session| session.kind == SessionKind::Launcher)?;
    if launcher_index + 1 < sessions.len() {
        let launcher = sessions.remove(launcher_index);
        sessions.push(launcher);
    }
    Some(sessions.len() - 1)
}

/// 底部文件管理器面板选中的页签
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BottomTab {
    Files,
    Commands,
}

pub(crate) struct TerminalBackgroundTexture {
    pub path: String,
    pub handle: egui::TextureHandle,
}

pub struct RShellApp {
    pub sessions: Vec<Session>,
    pub active: usize,
    next_seq: usize,
    pub bottom_tab: BottomTab,
    pub cmd_input: String,
    // 已保存的连接
    pub config: AppConfig,
    // 命令输入历史
    pub cmd_history: Vec<String>,
    pub cmd_hist_idx: Option<usize>,
    pub cmd_history_draft: Option<String>,
    // 新建常用命令
    pub command_new_open: bool,
    pub command_new_name: String,
    pub command_new_value: String,
    pub command_new_error: Option<String>,
    // 本机监控采样
    local_metrics_rx: mpsc::Receiver<Arc<SystemMetrics>>,
    local_sampling_enabled: Arc<AtomicBool>,
    /// 侧栏进程列表排序列(点击表头切换)
    pub proc_sort: ProcSort,
    // 当前会话的连接设置窗口
    pub show_session_edit: bool,
    /// 打开连接设置时绑定的稳定会话 ID，切换标签不会改变编辑目标。
    pub edit_session_id: Option<usize>,
    pub edit_alias: String,
    pub edit_group: String,
    pub edit_tags: String,
    pub edit_host: String,
    pub edit_port: String,
    pub edit_user: String,
    pub edit_pass: String,
    pub edit_key_mode: crate::ui::conn_library::LcKeyMode,
    pub edit_key_path: String,
    pub edit_remember: bool,
    pub edit_error: Option<String>,
    /// 设置窗口是否显示
    pub show_settings: bool,
    /// 设置窗口当前选中的页面
    pub settings_sub: Option<crate::ui::settings::SettingsPage>,
    /// 设置保存/迁移错误
    pub settings_error: Option<String>,
    /// 平台原生的软件更新后端及最近一次状态。
    pub(crate) updater: crate::update::UpdateManager,
    /// 终端背景图纹理缓存；路径改变时重新加载。
    pub(crate) terminal_background_texture: Option<TerminalBackgroundTexture>,
    /// 已经尝试过加载的路径，避免无效路径每帧反复读盘。
    pub(crate) terminal_background_load_attempt: Option<String>,
    /// 后台读取、解码背景图；纹理上传仍在 UI 线程完成。
    pub(crate) terminal_background_rx:
        Option<mpsc::Receiver<(String, Result<egui::ColorImage, String>)>>,
    /// 背景图片读取错误，仅在设置页提示。
    pub(crate) terminal_background_error: Option<String>,
    /// 连接库窗口
    pub show_library: bool,
    /// 连接库选中的文件夹(None = 全部)
    pub lib_folder: Option<String>,
    /// 连接库中处于折叠状态的文件夹路径
    pub lib_collapsed_folders: HashSet<String>,
    /// 连接库选中的连接(按 name)
    pub lib_conn: Option<String>,
    /// 连接库错误提示
    pub lib_error: Option<String>,
    /// 连接库操作成功提示
    pub lib_notice: Option<String>,
    // ---- 连接库:导出 ----
    pub connection_export_scope: Option<crate::export::ConnectionExportScope>,
    pub connection_export_encrypted: bool,
    pub connection_export_preserve_folders: bool,
    pub connection_export_password: String,
    pub connection_export_password_confirm: String,
    pub connection_export_error: Option<String>,
    pub connection_import: Option<crate::export::ConnectionImportState>,
    // ---- 在线预览/编辑 ----
    /// 内建文件编辑器(独立原生窗口,多文件标签)
    pub editor: crate::ui::file_editor::EditorState,
    /// 已发出的 ReadFile(探测/全量读),等 FileContent 事件
    pub preview_pending: Option<crate::ui::file_editor::PendingRead>,
    /// 内容嗅探无法判定,已发出 DetectKind,等 KindDetected 事件
    pub kind_pending: Option<crate::ui::file_editor::PendingKind>,
    /// 大文件打开确认对话框(>10MB)
    pub preview_confirm: Option<crate::session::files::FileEntry>,
    /// 二进制/错误提示对话框
    pub preview_notice: Option<crate::ui::file_editor::PreviewNotice>,
    /// 外部程序打开:等待临时下载完成
    pub external_pending: Option<crate::ui::file_editor::PendingExternal>,
    // ---- 连接库:连接编辑 ----
    pub lc_open: bool,
    pub lc_orig: Option<String>,
    pub lc_alias: String,
    pub lc_group: String,
    pub lc_tags: String,
    pub lc_host: String,
    pub lc_port: String,
    pub lc_user: String,
    pub lc_pass: String,
    pub lc_remember: bool,
    pub lc_key_mode: crate::ui::conn_library::LcKeyMode,
    pub lc_key_path: String,
    pub lc_error: Option<String>,
    // ---- 连接库:文件夹新建/重命名/删除 ----
    pub folder_new_open: bool,
    pub folder_new_name: String,
    pub folder_new_parent: String,
    pub folder_rename_from: Option<String>,
    pub folder_rename_to: String,
    pub folder_delete_pending: Option<String>,
    // ---- 连接库:标签快编 / 移动到文件夹 ----
    pub tag_edit_name: Option<String>,
    pub tag_edit_value: String,
    pub move_folder_name: Option<String>,
    /// 密钥管理窗口
    pub show_keys: bool,
    // ---- 密钥管理:生成/登记/编辑 ----
    pub key_form_open: bool,
    pub key_form_mode: crate::ui::conn_library::KeyFormMode,
    pub key_form_name: String,
    pub key_form_path: String,
    pub key_form_pass: String,
    pub key_form_algo: crate::keys::KeyAlgo,
    pub key_form_error: Option<String>,
    /// 密钥表单由连接编辑窗口打开；创建成功后自动选为该连接的认证密钥
    pub key_form_assign_to_lc: bool,
    /// 密钥表单由当前会话设置窗口打开；创建成功后自动选为该连接的认证密钥
    pub key_form_assign_to_edit: bool,
    pub key_sel: Option<String>,
    /// 主题偏好
    pub theme_pref: egui::ThemePreference,
}

impl RShellApp {
    #[cfg(test)]
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::new_with_config(cc, config::load())
    }

    pub fn new_with_config(cc: &eframe::CreationContext<'_>, mut loaded_config: AppConfig) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        #[cfg(target_os = "macos")]
        crate::macos_dock::install_window_menu();
        install_app_fonts(&cc.egui_ctx, &loaded_config.ssh_terminal.font_family);
        if !crate::terminal_font_families()
            .iter()
            .any(|family| family == &loaded_config.ssh_terminal.font_family)
        {
            loaded_config.ssh_terminal.font_family = "Hack".into();
        }
        let theme_pref = match loaded_config.theme_mode {
            config::ThemeMode::System => egui::ThemePreference::System,
            config::ThemeMode::Dark => egui::ThemePreference::Dark,
            config::ThemeMode::Light => egui::ThemePreference::Light,
        };
        cc.egui_ctx.set_theme(theme_pref);
        crate::ui::settings::apply_native_window_theme(&cc.egui_ctx, loaded_config.theme_mode);
        crate::ui::settings::apply_color_scheme(&cc.egui_ctx, loaded_config.color_scheme);
        crate::secrets::configure(loaded_config.password_storage);
        let updater = crate::update::UpdateManager::new(&loaded_config.updates);
        let config_load_error = config::take_load_error();
        let (local_metrics_tx, local_metrics_rx) = mpsc::sync_channel(1);
        let local_sampling_enabled = Arc::new(AtomicBool::new(false));
        let sampler_enabled = Arc::downgrade(&local_sampling_enabled);
        let sampler_ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            let mut sampler = LocalSampler::default();
            while let Some(enabled) = sampler_enabled.upgrade() {
                if !enabled.load(Ordering::Relaxed) {
                    drop(enabled);
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    continue;
                }
                drop(enabled);
                match local_metrics_tx.try_send(Arc::new(sampler.sample())) {
                    Ok(()) => sampler_ctx.request_repaint(),
                    Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        });
        Self {
            // 启动即显示保存连接列表，双击后在当前标签内建立 SSH 会话。
            sessions: vec![Session::launcher(1)],
            active: 0,
            next_seq: 2,
            bottom_tab: BottomTab::Files,
            cmd_input: String::new(),
            config: loaded_config,
            cmd_history: Vec::new(),
            cmd_hist_idx: None,
            cmd_history_draft: None,
            command_new_open: false,
            command_new_name: String::new(),
            command_new_value: String::new(),
            command_new_error: None,
            local_metrics_rx,
            local_sampling_enabled,
            proc_sort: ProcSort::default(),
            show_session_edit: false,
            edit_session_id: None,
            edit_alias: String::new(),
            edit_group: String::new(),
            edit_tags: String::new(),
            edit_host: String::new(),
            edit_port: "22".into(),
            edit_user: String::new(),
            edit_pass: String::new(),
            edit_key_mode: crate::ui::conn_library::LcKeyMode::Password,
            edit_key_path: String::new(),
            edit_remember: true,
            edit_error: None,
            show_settings: false,
            settings_sub: None,
            settings_error: None,
            updater,
            terminal_background_texture: None,
            terminal_background_load_attempt: None,
            terminal_background_rx: None,
            terminal_background_error: None,
            show_library: false,
            lib_folder: None,
            lib_collapsed_folders: HashSet::new(),
            lib_conn: None,
            lib_error: config_load_error,
            lib_notice: None,
            connection_export_scope: None,
            connection_export_encrypted: true,
            connection_export_preserve_folders: true,
            connection_export_password: String::new(),
            connection_export_password_confirm: String::new(),
            connection_export_error: None,
            connection_import: None,
            editor: crate::ui::file_editor::EditorState::default(),
            preview_pending: None,
            kind_pending: None,
            preview_confirm: None,
            preview_notice: None,
            external_pending: None,
            lc_open: false,
            lc_orig: None,
            lc_alias: String::new(),
            lc_group: String::new(),
            lc_tags: String::new(),
            lc_host: String::new(),
            lc_port: "22".into(),
            lc_user: "root".into(),
            lc_pass: String::new(),
            lc_remember: true,
            lc_key_mode: crate::ui::conn_library::LcKeyMode::Password,
            lc_key_path: String::new(),
            lc_error: None,
            folder_new_open: false,
            folder_new_name: String::new(),
            folder_new_parent: String::new(),
            folder_rename_from: None,
            folder_rename_to: String::new(),
            folder_delete_pending: None,
            tag_edit_name: None,
            tag_edit_value: String::new(),
            move_folder_name: None,
            show_keys: false,
            key_form_open: false,
            key_form_mode: crate::ui::conn_library::KeyFormMode::Generate,
            key_form_name: String::new(),
            key_form_path: String::new(),
            key_form_pass: String::new(),
            key_form_algo: crate::keys::KeyAlgo::Ed25519,
            key_form_error: None,
            key_form_assign_to_lc: false,
            key_form_assign_to_edit: false,
            key_sel: None,
            theme_pref,
        }
    }

    /// 记录一条命令历史(去重连续重复)
    pub fn push_cmd_history(&mut self, cmd: &str) {
        if !cmd.is_empty() && self.cmd_history.last().map(|s| s.as_str()) != Some(cmd) {
            self.cmd_history.push(cmd.to_string());
            if self.cmd_history.len() > 500 {
                self.cmd_history.remove(0);
            }
        }
        self.cmd_hist_idx = None;
        self.cmd_history_draft = None;
    }

    /// 新建本机会话标签
    pub fn open_local(&mut self, ctx: &egui::Context) {
        if self
            .sessions
            .get(self.active)
            .is_some_and(|s| s.kind == SessionKind::Launcher)
        {
            let id = self.sessions[self.active].id;
            self.sessions[self.active] = Session::local(ctx, id);
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.sessions.push(Session::local(ctx, seq));
        self.active = self.sessions.len() - 1;
    }

    /// 新建 SSH 会话标签
    pub fn open_ssh(&mut self, ctx: &egui::Context, params: SshParams) {
        let connection_name = crate::secrets::account_for(&params.user, &params.host, params.port);
        if self.config.record_connection_use(&connection_name) {
            let _ = crate::config::save(&self.config);
        }
        if self
            .sessions
            .get(self.active)
            .is_some_and(|s| s.kind == SessionKind::Launcher)
        {
            let id = self.sessions[self.active].id;
            self.sessions[self.active] = Session::ssh(ctx, id, params);
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.sessions.push(Session::ssh(ctx, seq, params));
        self.active = self.sessions.len() - 1;
    }

    /// 激活已有 SSH 启动页；不存在时才新建。
    pub fn open_ssh_launcher(&mut self) {
        if let Some(index) = move_existing_launcher_to_end(&mut self.sessions) {
            self.active = index;
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.sessions.push(Session::launcher(seq));
        self.active = self.sessions.len() - 1;
    }

    /// 在当前启动页中连接一条已保存的 SSH；失败信息保留在当前页。
    pub(crate) fn connect_saved_from_launcher(&mut self, ctx: &egui::Context, name: &str) {
        let Some(connection) = self
            .config
            .connections
            .iter()
            .find(|connection| connection.name == name)
            .cloned()
        else {
            if let Some(session) = self.sessions.get_mut(self.active) {
                session.error = Some("保存的连接已不存在".into());
            }
            return;
        };
        match crate::ui::connect_dialog::params_from_saved(&connection, &self.config) {
            Ok(params) => {
                let title = connection.session_title();
                self.open_ssh(ctx, params);
                if let Some(session) = self.sessions.get_mut(self.active) {
                    session.title = title;
                }
            }
            Err(error) => {
                if let Some(session) = self.sessions.get_mut(self.active) {
                    session.error = Some(error);
                }
            }
        }
    }

    pub(crate) fn active_is_launcher(&self) -> bool {
        self.sessions
            .get(self.active)
            .is_some_and(|session| session.kind == SessionKind::Launcher)
    }

    /// 当前标签对应的数据会话下标:SysInfo 标签解析到源会话
    pub fn effective_session_idx(&self) -> Option<usize> {
        let s = self.sessions.get(self.active)?;
        match s.kind {
            SessionKind::Launcher => None,
            SessionKind::SysInfo(src_id) => self.sessions.iter().position(|s| s.id == src_id),
            _ => Some(self.active),
        }
    }

    pub fn effective_session(&self) -> Option<&Session> {
        let idx = self.effective_session_idx()?;
        self.sessions.get(idx)
    }

    /// 为当前会话打开(或激活已存在的)系统信息标签页
    pub fn open_sysinfo_tab(&mut self) {
        let Some(src) = self.sessions.get(self.active) else {
            return;
        };
        if matches!(src.kind, SessionKind::Launcher | SessionKind::SysInfo(_)) {
            return;
        }
        let (src_id, src_host) = (src.id, src.host.clone());
        if let Some(i) = self
            .sessions
            .iter()
            .position(|s| s.kind == SessionKind::SysInfo(src_id))
        {
            self.active = i;
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.sessions.push(Session::sysinfo(seq, src_id, &src_host));
        self.active = self.sessions.len() - 1;
    }
}

impl eframe::App for RShellApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 记录用户调整后的主窗口内容尺寸；退出时统一写入配置。
        if let Some(rect) = ui.ctx().input(|input| input.viewport().inner_rect) {
            let size = rect.size();
            if size.x.is_finite() && size.y.is_finite() && size.x >= 1024.0 && size.y >= 640.0 {
                self.config.window_width = Some(size.x);
                self.config.window_height = Some(size.y);
            }
        }

        // fullsize_content_view 会让内容延伸到 macOS 原生标题栏下方。
        // 预留标题栏高度并用与窗体一致的 panel_fill 填充，避免控件与交通灯重叠。
        #[cfg(target_os = "macos")]
        {
            let titlebar_fill = ui.visuals().panel_fill;
            egui::Panel::top("native_titlebar_background")
                .exact_size(28.0)
                .frame(egui::Frame::new().fill(titlebar_fill))
                .show(ui, |_ui| {});
        }

        // 1. 喂所有会话的终端数据,取走监控指标与文件服务事件。只有当前实际
        // 展示的会话（系统信息页映射到其源会话）保持 1 秒监控频率，
        // 其余会话暂停监控并在重新激活时立即采样。
        let monitored_idx = self.effective_session_idx();
        for (idx, session) in self.sessions.iter_mut().enumerate() {
            if let Some(term) = session.term.as_mut() {
                term.set_monitor_active(monitored_idx == Some(idx));
                term.poll();
                if let Some(m) = term.take_metrics() {
                    session.monitor.update(m);
                }
                for e in term.take_sftp() {
                    session.fm.handle_event(e);
                }
                for p in term.take_hostkey() {
                    session.pending_hostkey = Some(p);
                }
            }
            session.fm.tick();
        }
        // 2. 本机指标由后台线程采样，UI 线程只消费最新快照。
        let has_local = monitored_idx.is_some_and(|idx| {
            self.sessions
                .get(idx)
                .is_some_and(|session| session.kind == SessionKind::Local)
        });
        self.local_sampling_enabled
            .store(has_local, Ordering::Relaxed);
        let latest_metrics = self.local_metrics_rx.try_iter().last();
        if let Some(m) = latest_metrics {
            for session in &mut self.sessions {
                if session.kind == SessionKind::Local {
                    session.monitor.update(Arc::clone(&m));
                }
            }
        }
        // 3. 顺序决定区域划分：左侧栏(满高)->右上标签栏->底部文件区->中央内容。
        // 系统信息页和 SSH 新建连接页使用完整的中央高度，不显示底部文件窗口。
        self.show_sidebar(ui);
        self.show_tab_bar(ui);
        // 调试开关:禁用部分 UI 用于定位拖拽冲突
        let active_is_sysinfo = self
            .sessions
            .get(self.active)
            .is_some_and(|session| matches!(session.kind, SessionKind::SysInfo(_)));
        let active_is_launcher = self.active_is_launcher();
        if !no_file_manager_debug() && !active_is_sysinfo && !active_is_launcher {
            self.show_file_manager(ui);
        }
        if !no_terminal_debug() {
            self.show_terminal(ui);
        }
        self.show_session_edit_dialog(ui);
        self.show_hostkey_dialog(ui);
        self.show_settings_dialog(ui);
        self.show_conn_library(ui);
        self.show_editor_viewport(ui);
        self.show_preview_dialogs(ui);
        // 侧栏拖拽覆盖条:必须在终端之后注册,否则把手右半边被终端 painter 吃掉
        self.show_sidebar_resize_grip(ui);
        #[cfg(target_os = "macos")]
        crate::macos_dock::restore_dock_window_order_if_pending();
    }

    fn on_exit(&mut self) {
        let _ = crate::config::save(&self.config);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_is_a_connectionless_tab() {
        let session = Session::launcher(7);
        assert!(session.kind == SessionKind::Launcher);
        assert_eq!(session.title, "新建连接");
        assert!(session.term.is_none());
        assert!(!session.is_connected());
        assert!(session.ssh_params().is_none());
    }

    #[test]
    fn existing_launcher_moves_to_the_end_instead_of_duplicating() {
        let mut sessions = vec![
            Session::launcher(10),
            Session::launcher(20),
            Session::launcher(30),
            Session::launcher(40),
        ];
        sessions[1].kind = SessionKind::Ssh;
        sessions[1].title = "ssh-a".into();
        sessions[3].kind = SessionKind::Ssh;
        sessions[3].title = "ssh-b".into();

        assert_eq!(move_existing_launcher_to_end(&mut sessions), Some(3));
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![10, 20, 40, 30]
        );
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.kind == SessionKind::Launcher)
                .count(),
            2
        );

        assert_eq!(move_existing_launcher_to_end(&mut sessions), Some(3));
        assert_eq!(sessions.last().map(|session| session.id), Some(30));
    }

    #[test]
    fn launcher_move_returns_none_when_all_tabs_are_connected() {
        let mut sessions = vec![Session::launcher(1), Session::launcher(2)];
        for session in &mut sessions {
            session.kind = SessionKind::Ssh;
        }
        assert_eq!(move_existing_launcher_to_end(&mut sessions), None);
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// 断开 → 未连接(网格保留);重连 → 恢复连接(本机会话起真实 pty)
    #[test]
    fn local_disconnect_reconnect() {
        let ctx = egui::Context::default();
        let mut s = Session::local(&ctx, 1);
        assert!(s.is_connected(), "新建本机会话应处于连接状态");
        s.disconnect();
        assert!(!s.is_connected());
        assert!(s.term.is_some(), "断开后终端网格应保留(滚回内容不丢)");
        s.reconnect(&ctx);
        assert!(s.is_connected(), "重连后应恢复连接");
        assert!(s.error.is_none());
    }

    /// 断开/重连后终端原有内容必须保留(复刻 FinalShell 的行为)
    #[test]
    fn reconnect_keeps_scrollback() {
        let ctx = egui::Context::default();
        let mut s = Session::local(&ctx, 1);
        let marker = "RSHELL_KEEP_ME_42";
        s.term.as_mut().unwrap().feed_str(&format!("{marker}\r\n"));
        s.disconnect();
        assert!(
            s.term.as_ref().unwrap().grid_text().contains(marker),
            "断开后网格内容应保留"
        );
        s.reconnect(&ctx);
        assert!(s.is_connected());
        assert!(
            s.term.as_ref().unwrap().grid_text().contains(marker),
            "重连后网格内容应保留"
        );
    }

    /// 编辑连接参数后标题使用 user@host，不添加序号前缀。
    #[test]
    fn update_params_uses_user_host_title() {
        let ctx = egui::Context::default();
        let params = |host: &str, user: &str| SshParams {
            host: host.into(),
            port: 22,
            user: user.into(),
            auth: crate::session::ssh::SshAuth::Password(String::new()),
        };
        let mut s = Session::ssh(&ctx, 3, params("a.com", "root"));
        assert_eq!(s.title, "root@a.com");
        s.update_ssh_params(params("b.com", "admin"));
        assert_eq!(s.title, "admin@b.com");
        assert_eq!(s.host, "b.com");
        assert_eq!(s.ssh_params().unwrap().user, "admin");

        s.title = "admin@生产服务器".into();
        s.update_ssh_params(params("c.com", "deploy"));
        assert_eq!(s.title, "deploy@生产服务器");
    }
}
