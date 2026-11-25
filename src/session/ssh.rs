// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! SSH 会话:russh 建立连接,独立线程跑 current_thread tokio runtime。
//! shell 通道的字流与本地 pty 一样经 SessionEvent/SessionCmd 与 UI 交互。

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh::ChannelMsg;
use russh::client::{self, Config, Handler};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, load_secret_key};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use zeroize::Zeroize;

use super::files::{
    FileEntry, SftpCmd, SftpEvent, SftpRx, TransferControl, WriteFailure, WriteFailureKind,
    WritePrivilege, WriteResult, sftp_channel,
};
use super::monitor::{
    MONITOR_DISKS_CMD, MONITOR_DYNAMIC_CMD, MONITOR_PROCESSES_CMD, MONITOR_STATIC_CMD,
    MonitorState, SystemMetrics,
};
use super::{
    CmdRx, DataTx, HostKeyPrompt, HostKeyVerdict, KeyboardInteractivePrompt,
    KeyboardInteractiveQuestion, KeyboardInteractiveVerdict, SessionChannels, SessionCmd,
    SessionEvent, cmd_channel, data_channel,
};

/// 认证方式
#[derive(Clone)]
pub enum SshAuth {
    Password(crate::secrets::SecretString),
    KeyFile {
        path: String,
        passphrase: Option<crate::secrets::SecretString>,
    },
}

#[derive(Clone)]
pub struct SshParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
}

const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const SSH_EXEC_TIMEOUT: Duration = Duration::from_secs(15);

fn ssh_client_config() -> Arc<Config> {
    Arc::new(Config {
        // 默认 2 MiB 窗口在高带宽、高 RTT 链路上容易限制吞吐；8 MiB 仍保持
        // 每个通道内存有界，同时给终端和 SFTP 留出足够的带宽时延积。
        window_size: 8 * 1024 * 1024,
        // TCP payload length is represented as u16 inside russh's transport path.
        // 64 KiB would overflow that ceiling and triggers a runtime warning.
        maximum_packet_size: u16::MAX as u32,
        channel_buffer_size: 256,
        nodelay: true,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    })
}

/// 建立 SSH 会话,立即返回通道;连接过程在后台线程进行,
/// 进度/错误以终端可显示的文本形式写进 Data 事件。
pub fn spawn(params: SshParams, cols: u16, rows: u16, ctx: egui::Context) -> SessionChannels {
    let (cmd_tx, cmd_rx) = cmd_channel();
    let (data_tx, data_rx) = data_channel(ctx.clone());
    let (event_tx, event_rx) = channel::<SessionEvent>();
    let (sftp_tx, sftp_rx) = sftp_channel();
    let sftp_tx_ret = sftp_tx.clone();
    let (monitor_active_tx, monitor_active_rx) = tokio::sync::watch::channel(true);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(run(
            params,
            cols,
            rows,
            cmd_rx,
            sftp_rx,
            data_tx,
            event_tx,
            ctx,
            monitor_active_rx,
        ));
    });
    SessionChannels {
        cmd_tx,
        data_rx,
        event_rx,
        sftp_tx: sftp_tx_ret,
        monitor_active_tx: Some(monitor_active_tx),
    }
}

/// 服务器指纹校验:known_hosts 记录命中则直接通过;
/// 新主机/指纹变更时向 UI 发确认请求并阻塞等待用户决定(TOFU)。
struct ClientHandler {
    host: String,
    port: u16,
    event_tx: Sender<SessionEvent>,
    data_tx: DataTx,
    verdict_tx: tokio::sync::mpsc::UnboundedSender<HostKeyVerdict>,
    verdict_rx: tokio::sync::mpsc::UnboundedReceiver<HostKeyVerdict>,
    trusted_server_fingerprint: Arc<Mutex<Option<String>>>,
    hostkey_prompt_active: Arc<std::sync::atomic::AtomicBool>,
    ctx: egui::Context,
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        use crate::known_hosts::{HostKeyStatus, KnownHostKey, KnownHosts};

        let fingerprint = server_public_key
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string();
        let openssh = server_public_key.to_openssh().unwrap_or_default();
        let algorithm = server_public_key.algorithm().to_string();

        let mut store = KnownHosts::load();
        match store.check(&self.host, self.port, &fingerprint) {
            HostKeyStatus::Match => {
                *self
                    .trusted_server_fingerprint
                    .lock()
                    .expect("trusted server fingerprint") = Some(fingerprint);
                Ok(true)
            }
            status => {
                self.hostkey_prompt_active
                    .store(true, std::sync::atomic::Ordering::Release);
                let prompt = HostKeyPrompt {
                    host: self.host.clone(),
                    port: self.port,
                    algorithm: algorithm.clone(),
                    fingerprint: fingerprint.clone(),
                    status,
                    reply: self.verdict_tx.clone(),
                };
                let _ = self
                    .event_tx
                    .send(SessionEvent::VerifyHostKey(Box::new(prompt)));
                self.ctx.request_repaint();
                // 异步等待，连接流程仍可被主命令通道取消。
                let verdict = tokio::time::timeout(
                    std::time::Duration::from_secs(300),
                    self.verdict_rx.recv(),
                )
                .await;
                self.hostkey_prompt_active
                    .store(false, std::sync::atomic::Ordering::Release);
                match verdict {
                    Ok(Some(HostKeyVerdict::AcceptAndSave)) => {
                        store.record(
                            &self.host,
                            self.port,
                            KnownHostKey {
                                algorithm,
                                fingerprint: fingerprint.clone(),
                                openssh,
                            },
                        );
                        if let Err(error) = store.save() {
                            let _ = self
                                .data_tx
                                .send(ansi(&format!(
                                    "\x1b[1;31m保存主机指纹失败：{error:#}\x1b[0m\r\n"
                                )))
                                .await;
                            Ok(false)
                        } else {
                            *self
                                .trusted_server_fingerprint
                                .lock()
                                .expect("trusted server fingerprint") = Some(fingerprint);
                            Ok(true)
                        }
                    }
                    Ok(Some(HostKeyVerdict::AcceptOnce)) => {
                        *self
                            .trusted_server_fingerprint
                            .lock()
                            .expect("trusted server fingerprint") = Some(fingerprint);
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
        }
    }
}

/// 批量传输连接不再次询问用户，而是只接受主连接已经验证通过的同一指纹。
/// 这样“仅本次接受”也能安全地建立第二条 TCP 连接。
struct TrustedClientHandler {
    fingerprint: String,
}

impl Handler for TrustedClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(server_public_key
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string()
            == self.fingerprint)
    }
}

struct BulkSftpConnection {
    _handle: client::Handle<TrustedClientHandler>,
    sftp: Arc<SftpSession>,
    last_used: Mutex<Instant>,
}

struct BulkSftpPool {
    params: SshParams,
    trusted_server_fingerprint: Arc<Mutex<Option<String>>>,
    connection: tokio::sync::Mutex<Option<Arc<BulkSftpConnection>>>,
    /// 服务器拒绝额外连接时避免每个传输都重复等待 TCP/认证超时。
    retry_after_failure: Mutex<Option<(Instant, String)>>,
}

impl BulkSftpPool {
    fn new(params: SshParams, trusted_server_fingerprint: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            params,
            trusted_server_fingerprint,
            connection: tokio::sync::Mutex::new(None),
            retry_after_failure: Mutex::new(None),
        }
    }

    async fn sftp(&self) -> Result<Arc<SftpSession>, String> {
        let mut connection = self.connection.lock().await;
        if let Some(connection) = connection.as_ref() {
            *connection.last_used.lock().expect("bulk connection clock") = Instant::now();
            return Ok(Arc::clone(&connection.sftp));
        }
        {
            let mut failure = self
                .retry_after_failure
                .lock()
                .expect("bulk connection retry state");
            if let Some((retry_at, error)) = failure.as_ref() {
                if *retry_at > Instant::now() {
                    return Err(format!("{error}（暂时直接回退主连接）"));
                }
                *failure = None;
            }
        }
        let fingerprint = self
            .trusted_server_fingerprint
            .lock()
            .expect("trusted server fingerprint")
            .clone()
            .ok_or_else(|| "主连接尚未完成服务器指纹验证".to_string())?;
        let created = self.connect(fingerprint).await;
        let created = match created {
            Ok(created) => created,
            Err(error) => {
                *self
                    .retry_after_failure
                    .lock()
                    .expect("bulk connection retry state") =
                    Some((Instant::now() + Duration::from_secs(60), error.clone()));
                return Err(error);
            }
        };
        let sftp = Arc::clone(&created.sftp);
        *connection = Some(created);
        Ok(sftp)
    }

    async fn connect(&self, fingerprint: String) -> Result<Arc<BulkSftpConnection>, String> {
        let handler = TrustedClientHandler { fingerprint };
        let mut handle = tokio::time::timeout(
            SSH_CONNECT_TIMEOUT,
            client::connect(
                ssh_client_config(),
                (self.params.host.as_str(), self.params.port),
                handler,
            ),
        )
        .await
        .map_err(|_| format!("独立传输连接超时（{} 秒）", SSH_CONNECT_TIMEOUT.as_secs()))?
        .map_err(|error| format!("建立独立传输连接失败: {error}"))?;
        authenticate_bulk(&mut handle, &self.params).await?;
        let sftp = Arc::new(open_sftp(&handle).await?);
        Ok(Arc::new(BulkSftpConnection {
            _handle: handle,
            sftp,
            last_used: Mutex::new(Instant::now()),
        }))
    }

    async fn close_if_idle(&self, idle_for: Duration) {
        let stale = {
            let mut connection = self.connection.lock().await;
            let should_close = connection.as_ref().is_some_and(|current| {
                current
                    .last_used
                    .lock()
                    .expect("bulk connection clock")
                    .elapsed()
                    >= idle_for
                    && Arc::strong_count(&current.sftp) == 1
            });
            should_close.then(|| connection.take()).flatten()
        };
        if let Some(connection) = stale {
            let _ = connection.sftp.close().await;
        }
    }
}

async fn authenticate_bulk(
    handle: &mut client::Handle<TrustedClientHandler>,
    params: &SshParams,
) -> Result<(), String> {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        match &params.auth {
            SshAuth::Password(password) => handle
                .authenticate_password(params.user.clone(), password.expose().to_owned())
                .await
                .map_err(|error| error.to_string()),
            SshAuth::KeyFile { path, passphrase } => {
                let key = load_secret_key(path, passphrase.as_ref().map(|value| value.expose()))
                    .map_err(|error| format!("加载独立传输连接私钥失败({path}): {error}"))?;
                let hash: Option<HashAlg> = handle
                    .best_supported_rsa_hash()
                    .await
                    .ok()
                    .flatten()
                    .flatten();
                handle
                    .authenticate_publickey(
                        params.user.clone(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
        }
    })
    .await
    .map_err(|_| "独立传输连接认证超时（30 秒）".to_string())??;
    if result.success() {
        Ok(())
    } else {
        Err(format!("独立传输连接认证失败: {result:?}"))
    }
}

fn ansi(s: &str) -> Vec<u8> {
    s.replace('\n', "\r\n").into_bytes()
}

/// SSH shell 建立前暂存 UI 命令。窗口尺寸只保留最新值；输入不能在连接阶段
/// 静默丢弃。返回 false 表示标签已关闭或用户取消连接。
fn stage_pre_shell_command(
    command: Option<SessionCmd>,
    cols: &mut u16,
    rows: &mut u16,
    pending_input: &mut VecDeque<Vec<u8>>,
) -> bool {
    match command {
        Some(SessionCmd::Resize {
            cols: next_cols,
            rows: next_rows,
        }) => {
            *cols = next_cols;
            *rows = next_rows;
            true
        }
        Some(SessionCmd::Input(data)) => {
            pending_input.push_back(data);
            true
        }
        Some(SessionCmd::Shutdown) | None => false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    params: SshParams,
    cols: u16,
    rows: u16,
    mut cmd_rx: CmdRx,
    sftp_rx: SftpRx,
    data_tx: DataTx,
    event_tx: Sender<SessionEvent>,
    ctx: egui::Context,
    monitor_active_rx: tokio::sync::watch::Receiver<bool>,
) {
    let (mut pty_cols, mut pty_rows) = (cols, rows);
    let mut pending_input = VecDeque::new();

    // 进度与错误直接打印到终端区
    macro_rules! status {
        ($($arg:tt)*) => {{
            let _ = data_tx.send(ansi(&format!("\x1b[36m{}\x1b[0m\r\n", format!($($arg)*)))).await;
        }};
    }
    macro_rules! fatal {
        ($($arg:tt)*) => {{
            let _ = data_tx.send(ansi(&format!("\x1b[1;31m{}\x1b[0m\r\n", format!($($arg)*)))).await;
            let _ = event_tx.send(SessionEvent::Exited);
            ctx.request_repaint();
            return;
        }};
    }

    status!("连接主机 {}:{} ...", params.host, params.port);

    let config = ssh_client_config();
    let trusted_server_fingerprint = Arc::new(Mutex::new(None));
    let hostkey_prompt_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (verdict_tx, verdict_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler = ClientHandler {
        host: params.host.clone(),
        port: params.port,
        event_tx: event_tx.clone(),
        data_tx: data_tx.clone(),
        verdict_tx,
        verdict_rx,
        trusted_server_fingerprint: Arc::clone(&trusted_server_fingerprint),
        hostkey_prompt_active: Arc::clone(&hostkey_prompt_active),
        ctx: ctx.clone(),
    };
    let connect = client::connect(config, (params.host.as_str(), params.port), handler);
    tokio::pin!(connect);
    let connect_timeout = tokio::time::sleep(SSH_CONNECT_TIMEOUT);
    tokio::pin!(connect_timeout);
    let mut handle = loop {
        tokio::select! {
            result = &mut connect => match result {
                Ok(handle) => break handle,
                Err(error) => fatal!("连接失败:{error}"),
            },
            () = &mut connect_timeout => {
                if hostkey_prompt_active.load(std::sync::atomic::Ordering::Acquire) {
                    connect_timeout.as_mut().reset(tokio::time::Instant::now() + SSH_CONNECT_TIMEOUT);
                    continue;
                }
                fatal!("连接超时（{} 秒）", SSH_CONNECT_TIMEOUT.as_secs());
            },
            command = cmd_rx.recv() => {
                if !stage_pre_shell_command(
                    command,
                    &mut pty_cols,
                    &mut pty_rows,
                    &mut pending_input,
                ) {
                    let _ = event_tx.send(SessionEvent::Exited);
                    ctx.request_repaint();
                    return;
                }
            }
        }
    };

    // ---- 认证 ----
    let auth_result = match &params.auth {
        SshAuth::Password(pass) => match tokio::time::timeout(
            Duration::from_secs(30),
            handle.authenticate_password(params.user.clone(), pass.expose().to_owned()),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => fatal!("认证超时（30 秒）"),
        },
        SshAuth::KeyFile { path, passphrase } => {
            match load_secret_key(path, passphrase.as_ref().map(|value| value.expose())) {
                Ok(key) => {
                    match tokio::time::timeout(Duration::from_secs(30), async {
                        let hash: Option<HashAlg> = handle
                            .best_supported_rsa_hash()
                            .await
                            .ok()
                            .flatten()
                            .flatten();
                        handle
                            .authenticate_publickey(
                                params.user.clone(),
                                PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                            )
                            .await
                    })
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => fatal!("认证超时（30 秒）"),
                    }
                }
                Err(e) => fatal!("加载私钥失败({path}):{e}"),
            }
        }
    };
    match auth_result {
        Ok(result) if result.success() => status!("认证成功"),
        Ok(result) if auth_result_allows_keyboard_interactive(&result) => {
            status!("服务器要求 Keyboard-Interactive/2FA 认证");
            match authenticate_keyboard_interactive(&mut handle, &params, &event_tx, &ctx).await {
                Ok(()) => status!("认证成功"),
                Err(error) => fatal!("交互式认证失败:{error}"),
            }
        }
        Ok(result) => fatal!("认证失败:{result:?}"),
        Err(e) => fatal!("认证出错:{e}"),
    }

    // TCP 连接完成到认证结束期间 UI 可能已经完成首次布局。把这段时间
    // 排队的 resize 合并到 PTY 初始尺寸，避免远端仍按 80 列而本地按实际
    // 列数渲染；这会直接破坏 readline 对跨行历史命令的清除和光标回退。
    loop {
        match cmd_rx.try_recv() {
            Ok(command) => {
                if !stage_pre_shell_command(
                    Some(command),
                    &mut pty_cols,
                    &mut pty_rows,
                    &mut pending_input,
                ) {
                    let _ = event_tx.send(SessionEvent::Exited);
                    ctx.request_repaint();
                    return;
                }
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                let _ = event_tx.send(SessionEvent::Exited);
                ctx.request_repaint();
                return;
            }
        }
    }

    // ---- shell 通道 ----
    let handle = Arc::new(handle);
    let mut channel =
        match tokio::time::timeout(SSH_EXEC_TIMEOUT, handle.channel_open_session()).await {
            Ok(Ok(channel)) => channel,
            Ok(Err(error)) => fatal!("打开通道失败:{error}"),
            Err(_) => fatal!("打开终端通道超时（{} 秒）", SSH_EXEC_TIMEOUT.as_secs()),
        };
    match tokio::time::timeout(
        SSH_EXEC_TIMEOUT,
        channel.request_pty(
            false,
            "xterm-256color",
            pty_cols as u32,
            pty_rows as u32,
            0,
            0,
            &[],
        ),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => fatal!("请求 PTY 失败:{error}"),
        Err(_) => fatal!("请求 PTY 超时（{} 秒）", SSH_EXEC_TIMEOUT.as_secs()),
    }
    match tokio::time::timeout(SSH_EXEC_TIMEOUT, channel.request_shell(false)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => fatal!("启动 shell 失败:{error}"),
        Err(_) => fatal!("启动 shell 超时（{} 秒）", SSH_EXEC_TIMEOUT.as_secs()),
    }
    while let Some(data) = pending_input.pop_front() {
        if channel.data_bytes(data).await.is_err() {
            let _ = event_tx.send(SessionEvent::Exited);
            ctx.request_repaint();
            return;
        }
    }

    // ---- 监控任务:同一连接上保持一条长期 exec 通道,1s 一轮 ----
    // shutdown 标志:主动断开时主循环置位,监控循环随之退出
    // (否则它持有的 Arc<Handle> 会让连接永远断不掉)
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mon_handle = handle.clone();
    let mon_tx = event_tx.clone();
    let mon_data_tx = data_tx.clone();
    let mon_ctx = ctx.clone();
    let mon_shutdown = shutdown.clone();
    tokio::spawn(async move {
        monitor_loop(
            mon_handle,
            mon_data_tx,
            mon_tx,
            mon_ctx,
            mon_shutdown,
            monitor_active_rx,
        )
        .await;
    });

    // ---- SFTP 任务 ----
    {
        let sftp_handle = handle.clone();
        let sftp_tx_ev = event_tx.clone();
        let sftp_ctx = ctx.clone();
        let bulk_params = params.clone();
        let bulk_trusted_fingerprint = Arc::clone(&trusted_server_fingerprint);
        tokio::spawn(async move {
            sftp_loop(
                sftp_handle,
                sftp_rx,
                sftp_tx_ev,
                sftp_ctx,
                bulk_params,
                bulk_trusted_fingerprint,
            )
            .await;
        });
    }

    // ---- 主循环:通道数据 <-> UI ----
    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        if send_terminal_data(&data_tx, data).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if send_terminal_data(&data_tx, data).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                        let _ = event_tx.send(SessionEvent::Exited);
                        ctx.request_repaint();
                        break;
                    }
                    _ => {}
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(SessionCmd::Input(bytes)) => {
                        if channel.data_bytes(bytes).await.is_err() {
                            let _ = event_tx.send(SessionEvent::Exited);
                            ctx.request_repaint();
                            break;
                        }
                    }
                    Some(SessionCmd::Resize { cols, rows }) => {
                        let _ = channel
                            .window_change(cols as u32, rows as u32, 0, 0)
                            .await;
                    }
                    Some(SessionCmd::Shutdown) => {
                        // 主动断开:置位 shutdown(监控循环退出)、断开连接并发 Exited
                        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
                        let _ = handle
                            .disconnect(russh::Disconnect::ByApplication, "user disconnect", "")
                            .await;
                        let _ = event_tx.send(SessionEvent::Exited);
                        ctx.request_repaint();
                        break;
                    }
                    None => break, // UI 关闭了标签页
                }
            }
        }
    }
}

async fn send_terminal_data(data_tx: &DataTx, data: bytes::Bytes) -> Result<(), ()> {
    for start in (0..data.len()).step_by(64 * 1024) {
        let end = (start + 64 * 1024).min(data.len());
        data_tx.send(data.slice(start..end)).await.map_err(|_| ())?;
    }
    Ok(())
}

fn auth_result_allows_keyboard_interactive(result: &client::AuthResult) -> bool {
    matches!(
        result,
        client::AuthResult::Failure {
            remaining_methods,
            ..
        } if remaining_methods.contains(&russh::MethodKind::KeyboardInteractive)
    )
}

fn looks_like_password_prompt(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    lower.contains("password") || lower.contains("passphrase") || prompt.contains("密码")
}

async fn authenticate_keyboard_interactive(
    handle: &mut client::Handle<ClientHandler>,
    params: &SshParams,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    use client::KeyboardInteractiveAuthResponse as Response;

    let saved_password = match &params.auth {
        SshAuth::Password(password) => Some(password.clone()),
        SshAuth::KeyFile { .. } => None,
    };
    let mut response = tokio::time::timeout(
        Duration::from_secs(30),
        handle.authenticate_keyboard_interactive_start(params.user.clone(), None::<String>),
    )
    .await
    .map_err(|_| "启动 Keyboard-Interactive 认证超时（30 秒）".to_string())?
    .map_err(|error| error.to_string())?;

    for _round in 0..16 {
        let responses = match response {
            Response::Success => return Ok(()),
            Response::Failure {
                remaining_methods,
                partial_success,
            } => {
                return Err(format!(
                    "服务器拒绝交互式认证（partial_success={partial_success}, remaining={remaining_methods:?}）"
                ));
            }
            Response::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                if prompts.len() > 32
                    || name
                        .len()
                        .saturating_add(instructions.len())
                        .saturating_add(
                            prompts
                                .iter()
                                .map(|prompt| prompt.prompt.len())
                                .sum::<usize>(),
                        )
                        > 64 * 1024
                {
                    return Err("服务器返回的交互式认证问题超过安全上限".to_string());
                }
                if prompts.is_empty() {
                    Vec::new()
                } else if prompts.len() == 1
                    && !prompts[0].echo
                    && looks_like_password_prompt(&prompts[0].prompt)
                    && saved_password.is_some()
                {
                    vec![
                        saved_password
                            .as_ref()
                            .expect("checked above")
                            .expose()
                            .to_owned(),
                    ]
                } else {
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    let mut questions = Vec::with_capacity(prompts.len());
                    let mut response_fields = Vec::with_capacity(prompts.len());
                    for prompt in prompts {
                        let suggested = (!prompt.echo
                            && looks_like_password_prompt(&prompt.prompt))
                        .then_some(saved_password.as_ref())
                        .flatten()
                        .map(|password| password.expose())
                        .unwrap_or_default();
                        questions.push(KeyboardInteractiveQuestion {
                            prompt: prompt.prompt,
                            echo: prompt.echo,
                        });
                        response_fields.push(zeroize::Zeroizing::new(suggested.to_owned()));
                    }
                    let expected_responses = questions.len();
                    event_tx
                        .send(SessionEvent::KeyboardInteractive(Box::new(
                            KeyboardInteractivePrompt {
                                host: params.host.clone(),
                                user: params.user.clone(),
                                name,
                                instructions,
                                questions,
                                responses: response_fields,
                                focus_first: true,
                                reply: reply_tx,
                            },
                        )))
                        .map_err(|_| "会话界面已关闭".to_string())?;
                    ctx.request_repaint();
                    match tokio::time::timeout(Duration::from_secs(300), reply_rx).await {
                        Ok(Ok(KeyboardInteractiveVerdict::Responses(values))) => {
                            if values.len() != expected_responses {
                                return Err(format!(
                                    "交互式认证回答数量不匹配：需要 {expected_responses}，收到 {}",
                                    values.len()
                                ));
                            }
                            values
                                .into_iter()
                                .map(|value| value.expose().to_owned())
                                .collect()
                        }
                        Ok(Ok(KeyboardInteractiveVerdict::Cancel)) => {
                            return Err("用户取消交互式认证".to_string());
                        }
                        Ok(Err(_)) => return Err("交互式认证窗口已关闭".to_string()),
                        Err(_) => return Err("等待交互式认证输入超时（300 秒）".to_string()),
                    }
                }
            }
        };
        response = tokio::time::timeout(
            Duration::from_secs(30),
            handle.authenticate_keyboard_interactive_respond(responses),
        )
        .await
        .map_err(|_| "提交 Keyboard-Interactive 响应超时（30 秒）".to_string())?
        .map_err(|error| error.to_string())?;
    }
    Err("服务器返回过多轮交互式认证问题".to_string())
}

const MONITOR_FRAME_BEGIN: &str = "__RSHELL_MONITOR_FRAME_BEGIN_7C2A__";
const MONITOR_FRAME_END: &str = "__RSHELL_MONITOR_FRAME_END_7C2A__";
const MONITOR_STREAM_BUFFER_LIMIT: usize = 2 * 1024 * 1024;

fn monitor_stream_command() -> String {
    format!(
        "LC_ALL=C; export LC_ALL; sample=0; while :; do echo {MONITOR_FRAME_BEGIN}; \
         {MONITOR_DYNAMIC_CMD} if [ $((sample % 5)) -eq 0 ]; then \
         {MONITOR_DISKS_CMD}{MONITOR_PROCESSES_CMD} fi; echo {MONITOR_FRAME_END}; \
         sample=$((sample + 1)); sleep 1; done"
    )
}

fn find_subslice(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 远端 stdout 可能在任意 SSH packet 边界被拆分，因此先在字节缓冲中组帧。
fn take_monitor_frame(buffer: &mut Vec<u8>) -> Option<String> {
    let begin_marker = MONITOR_FRAME_BEGIN.as_bytes();
    let end_marker = MONITOR_FRAME_END.as_bytes();
    let begin = find_subslice(buffer, begin_marker)?;
    let content_start = begin + begin_marker.len();
    let relative_end = find_subslice(&buffer[content_start..], end_marker)?;
    let content_end = content_start + relative_end;
    let frame = String::from_utf8_lossy(&buffer[content_start..content_end])
        .trim_matches(['\r', '\n'])
        .to_string();
    buffer.drain(..content_end + end_marker.len());
    Some(frame)
}

enum MonitorStreamEvent {
    Sample(String),
    Paused,
    Stopped,
}

struct MonitorStream {
    channel: russh::Channel<client::Msg>,
    buffer: Vec<u8>,
}

impl MonitorStream {
    async fn open(handle: &client::Handle<ClientHandler>) -> Result<Self, String> {
        let channel = tokio::time::timeout(SSH_EXEC_TIMEOUT, handle.channel_open_session())
            .await
            .map_err(|_| format!("打开监控通道超时（{} 秒）", SSH_EXEC_TIMEOUT.as_secs()))?
            .map_err(|error| format!("打开监控通道失败: {error}"))?;
        let command = monitor_stream_command();
        tokio::time::timeout(SSH_EXEC_TIMEOUT, channel.exec(false, command))
            .await
            .map_err(|_| format!("启动监控命令超时（{} 秒）", SSH_EXEC_TIMEOUT.as_secs()))?
            .map_err(|error| format!("启动监控命令失败: {error}"))?;
        Ok(Self {
            channel,
            buffer: Vec::with_capacity(32 * 1024),
        })
    }

    async fn next(
        &mut self,
        active_rx: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<MonitorStreamEvent, String> {
        if let Some(frame) = take_monitor_frame(&mut self.buffer) {
            return Ok(MonitorStreamEvent::Sample(frame));
        }
        loop {
            tokio::select! {
                changed = active_rx.changed() => {
                    if changed.is_err() {
                        return Ok(MonitorStreamEvent::Stopped);
                    }
                    if !*active_rx.borrow() {
                        return Ok(MonitorStreamEvent::Paused);
                    }
                }
                message = self.channel.wait() => {
                    match message {
                        Some(ChannelMsg::Data { data }) => {
                            self.buffer.extend_from_slice(&data);
                            if self.buffer.len() > MONITOR_STREAM_BUFFER_LIMIT {
                                return Err("监控输出超过安全上限".to_string());
                            }
                            if let Some(frame) = take_monitor_frame(&mut self.buffer) {
                                return Ok(MonitorStreamEvent::Sample(frame));
                            }
                        }
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                            return Err("监控通道已关闭".to_string());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn close(self) {
        let _ = self.channel.close().await;
    }
}

/// 通过一条长期 exec 通道每秒采集系统指标；后台标签会关闭通道完全暂停。
async fn monitor_loop(
    handle: Arc<client::Handle<ClientHandler>>,
    data_tx: DataTx,
    event_tx: Sender<SessionEvent>,
    ctx: egui::Context,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut active_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut state = MonitorState::default();
    let mut consecutive_errors = 0u32;
    let mut static_metrics: Option<SystemMetrics> = None;
    let mut disks = Vec::new();
    let mut processes = Vec::new();
    let mut next_rtt_sample = Instant::now();
    let mut measured_rtt = 0u32;
    let mut stream: Option<MonitorStream> = None;
    // 等 shell 先起来
    tokio::time::sleep(Duration::from_millis(500)).await;
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if !*active_rx.borrow() {
            if let Some(stream) = stream.take() {
                stream.close().await;
            }
            if !wait_for_monitor_reactivation(&mut active_rx).await {
                break;
            }
            continue;
        }
        // 静态信息只在连接建立后获取一次；首次失败则随下一轮继续重试。
        if static_metrics.is_none()
            && let Ok(text) = run_exec(&handle, MONITOR_STATIC_CMD).await
        {
            static_metrics = Some(MonitorState::default().update(&text, 0));
        }

        // exec 耗时包含远端 ps/df 的执行时间，不是真实网络 RTT。低频发送 SSH
        // ping 单独测量，避免为了展示 RTT 每秒再增加一次网络往返。
        let now = Instant::now();
        if now >= next_rtt_sample {
            let ping_started = Instant::now();
            if tokio::time::timeout(Duration::from_secs(5), handle.send_ping())
                .await
                .is_ok_and(|result| result.is_ok())
            {
                measured_rtt = ping_started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            }
            next_rtt_sample = Instant::now() + Duration::from_secs(5);
        }

        if stream.is_none() {
            match MonitorStream::open(&handle).await {
                Ok(opened) => stream = Some(opened),
                Err(error) => {
                    consecutive_errors = consecutive_errors.saturating_add(1);
                    eprintln!(
                        "SSH 监控通道启动失败（第 {consecutive_errors} 次，将自动重试）：{error}"
                    );
                    if consecutive_errors == 1 {
                        let _ = data_tx
                            .send(ansi("\x1b[33m系统监控暂时不可用，正在自动重试\x1b[0m\r\n"))
                            .await;
                    }
                    let delay = 1u64 << consecutive_errors.min(5);
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    continue;
                }
            }
        }

        let stream_event = stream
            .as_mut()
            .expect("监控通道刚刚已创建")
            .next(&mut active_rx)
            .await;
        match stream_event {
            Ok(MonitorStreamEvent::Sample(text)) => {
                if consecutive_errors > 0 {
                    let _ = data_tx
                        .send(ansi("\x1b[36m系统监控已恢复\x1b[0m\r\n"))
                        .await;
                }
                consecutive_errors = 0;
                let mut metrics = state.update(&text, measured_rtt);
                let sample_disks = text.lines().any(|line| line == "=DF");
                let sample_processes = text.lines().any(|line| line == "=PS");
                if let Some(cached) = &static_metrics {
                    metrics.os_pretty.clone_from(&cached.os_pretty);
                    metrics.kernel_name.clone_from(&cached.kernel_name);
                    metrics.kernel_release.clone_from(&cached.kernel_release);
                    metrics.arch.clone_from(&cached.arch);
                    metrics.hostname.clone_from(&cached.hostname);
                    metrics.cpu_model.clone_from(&cached.cpu_model);
                    metrics.cpu_cores = cached.cpu_cores;
                    metrics.cpu_mhz = cached.cpu_mhz;
                    metrics.cpu_cache_kb = cached.cpu_cache_kb;
                    metrics.cpu_bogomips = cached.cpu_bogomips;
                }
                if sample_disks {
                    disks.clone_from(&metrics.disks);
                } else {
                    metrics.disks.clone_from(&disks);
                }
                if sample_processes {
                    processes.clone_from(&metrics.procs);
                } else {
                    metrics.procs.clone_from(&processes);
                }
                if event_tx
                    .send(SessionEvent::Metrics(Arc::new(metrics)))
                    .is_err()
                {
                    break;
                }
                if *active_rx.borrow() {
                    ctx.request_repaint();
                }
            }
            Ok(MonitorStreamEvent::Paused) => {
                if let Some(stream) = stream.take() {
                    stream.close().await;
                }
            }
            Ok(MonitorStreamEvent::Stopped) => break,
            Err(error) => {
                if let Some(stream) = stream.take() {
                    stream.close().await;
                }
                consecutive_errors = consecutive_errors.saturating_add(1);
                eprintln!("SSH 监控采样失败（第 {consecutive_errors} 次，将自动重试）：{error}");
                if consecutive_errors == 1 {
                    let _ = data_tx
                        .send(ansi("\x1b[33m系统监控暂时不可用，正在自动重试\x1b[0m\r\n"))
                        .await;
                }
                let delay = 1u64 << consecutive_errors.min(5);
                tokio::time::sleep(Duration::from_secs(delay)).await;
                continue;
            }
        }
    }
    if let Some(stream) = stream {
        stream.close().await;
    }
}

/// 后台标签不发起任何采样，重新变为前台时立即返回。
async fn wait_for_monitor_reactivation(active_rx: &mut tokio::sync::watch::Receiver<bool>) -> bool {
    while !*active_rx.borrow() {
        if active_rx.changed().await.is_err() {
            return false;
        }
    }
    true
}

/// 执行一条命令并收集全部 stdout
async fn run_exec(handle: &client::Handle<ClientHandler>, cmd: &str) -> Result<String, String> {
    const OUTPUT_LIMIT: usize = 256 * 1024;
    tokio::time::timeout(SSH_EXEC_TIMEOUT, async {
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|error| error.to_string())?;
        channel
            .exec(false, cmd)
            .await
            .map_err(|error| error.to_string())?;
        let mut out = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    if out.len().saturating_add(data.len()) > OUTPUT_LIMIT {
                        let _ = channel.close().await;
                        return Err(format!(
                            "远端命令输出超过 {} KiB 安全上限",
                            OUTPUT_LIMIT / 1024
                        ));
                    }
                    out.extend_from_slice(&data);
                }
                ChannelMsg::Eof => {}
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    })
    .await
    .map_err(|_| format!("远端命令执行超时（{} 秒）", SSH_EXEC_TIMEOUT.as_secs()))?
}

struct ExecOutput {
    status: Option<u32>,
    stdout: String,
    stderr: String,
}

/// 执行受控命令并取得退出码。stdin 仅用于 sudo 的标准输入，调用方负责清零。
/// 输出设硬上限，避免异常远端命令无限占用内存。
async fn run_exec_capture(
    handle: &client::Handle<ClientHandler>,
    cmd: &str,
    stdin: Option<&[u8]>,
) -> Result<ExecOutput, String> {
    tokio::time::timeout(
        Duration::from_secs(120),
        run_exec_capture_inner(handle, cmd, stdin),
    )
    .await
    .map_err(|_| "远端命令执行超时（120 秒）".to_string())?
}

async fn run_exec_capture_inner(
    handle: &client::Handle<ClientHandler>,
    cmd: &str,
    stdin: Option<&[u8]>,
) -> Result<ExecOutput, String> {
    const OUTPUT_LIMIT: usize = 64 * 1024;
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("打开远端命令通道失败: {e}"))?;
    channel
        .exec(true, cmd)
        .await
        .map_err(|e| format!("启动远端命令失败: {e}"))?;
    if let Some(stdin) = stdin {
        channel
            .data_bytes(stdin.to_vec())
            .await
            .map_err(|e| format!("发送管理员认证信息失败: {e}"))?;
    }
    channel
        .eof()
        .await
        .map_err(|e| format!("关闭远端命令输入失败: {e}"))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => append_bounded(&mut stdout, &data, OUTPUT_LIMIT),
            ChannelMsg::ExtendedData { data, .. } => {
                append_bounded(&mut stderr, &data, OUTPUT_LIMIT)
            }
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Eof => {}
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(ExecOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn append_bounded(target: &mut Vec<u8>, data: &[u8], limit: usize) {
    let remaining = limit.saturating_sub(target.len());
    target.extend_from_slice(&data[..data.len().min(remaining)]);
}

// ---------------------------------------------------------------------------
// SFTP
// ---------------------------------------------------------------------------

async fn sftp_loop(
    handle: Arc<client::Handle<ClientHandler>>,
    mut rx: SftpRx,
    event_tx: Sender<SessionEvent>,
    ctx: egui::Context,
    bulk_params: SshParams,
    trusted_server_fingerprint: Arc<Mutex<Option<String>>>,
) {
    // 打开 SFTP 子系统;失败则排空命令并逐一报错
    let sftp = match open_sftp(&handle).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::SetupFailed(format!(
                "SFTP 初始化失败:{e}"
            ))));
            ctx.request_repaint();
            while let Some(cmd) = rx.recv().await {
                let event = failed_sftp_command(cmd, "SFTP 不可用");
                let _ = event_tx.send(SessionEvent::Sftp(event));
                ctx.request_repaint();
            }
            return;
        }
    };

    // 上报会话默认目录(通常是用户主目录)
    if let Ok(home) = sftp.canonicalize(".").await {
        let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::HomeDir(home)));
        ctx.request_repaint();
    }

    let controls: Arc<Mutex<HashMap<u64, Arc<TransferControl>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let transfer_slots = Arc::new(tokio::sync::Semaphore::new(2));
    let bulk_pool = Arc::new(BulkSftpPool::new(bulk_params, trusted_server_fingerprint));
    let read_operation_slots = Arc::new(tokio::sync::Semaphore::new(8));
    let mutation_slots = Arc::new(tokio::sync::Semaphore::new(1));
    let latest_list_generations = Arc::new(Mutex::new(HashMap::<String, u64>::new()));
    let mut next_list_generation = 1u64;
    let cleanup_start = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut bulk_cleanup = tokio::time::interval_at(cleanup_start, Duration::from_secs(60));
    loop {
        let cmd = tokio::select! {
            command = rx.recv() => {
                let Some(command) = command else { break };
                command
            }
            _ = bulk_cleanup.tick() => {
                let cleanup_pool = Arc::clone(&bulk_pool);
                tokio::spawn(async move {
                    cleanup_pool.close_if_idle(Duration::from_secs(120)).await;
                });
                continue;
            }
        };
        match cmd {
            SftpCmd::CancelTransfer { id } => {
                if let Some(control) = controls.lock().expect("transfer controls").get(&id) {
                    control.cancel();
                }
                continue;
            }
            SftpCmd::PauseTransfer { id } => {
                if let Some(control) = controls.lock().expect("transfer controls").get(&id) {
                    control.set_paused(true);
                }
                continue;
            }
            SftpCmd::ResumeTransfer { id } => {
                if let Some(control) = controls.lock().expect("transfer controls").get(&id) {
                    control.set_paused(false);
                }
                continue;
            }
            cmd @ (SftpCmd::Download { .. }
            | SftpCmd::DownloadDir { .. }
            | SftpCmd::UploadDir { .. }
            | SftpCmd::Upload { .. }) => {
                let id = transfer_command_id(&cmd);
                let control = Arc::new(TransferControl::default());
                controls
                    .lock()
                    .expect("transfer controls")
                    .insert(id, Arc::clone(&control));
                let task_sftp = Arc::clone(&sftp);
                let task_bulk_pool = Arc::clone(&bulk_pool);
                let task_event_tx = event_tx.clone();
                let task_ctx = ctx.clone();
                let task_controls = Arc::clone(&controls);
                let task_slots = Arc::clone(&transfer_slots);
                tokio::spawn(async move {
                    let permit = tokio::select! {
                        permit = task_slots.acquire_owned() => match permit {
                            Ok(permit) => Some(permit),
                            Err(_) => unreachable!("transfer semaphore is owned by the session"),
                        },
                        () = control.wait_canceled() => None,
                    };
                    let result = if let Some(_permit) = permit {
                        let transfer_sftp = tokio::select! {
                            result = task_bulk_pool.sftp() => Some(match result {
                                Ok(sftp) => sftp,
                                Err(error) => {
                                    // 部分服务器限制并发 SSH 连接；这种情况下仍可在主
                                    // 连接上传输，只是无法获得终端流量隔离。
                                    eprintln!("独立传输连接不可用，回退主连接: {error}");
                                    task_sftp
                                }
                            }),
                            () = control.wait_canceled() => None,
                        };
                        if let Some(transfer_sftp) = transfer_sftp {
                            run_remote_transfer(
                                transfer_sftp,
                                cmd,
                                Arc::clone(&control),
                                &task_event_tx,
                                &task_ctx,
                            )
                            .await
                        } else {
                            failed_sftp_command(cmd, "传输已取消")
                        }
                    } else {
                        failed_sftp_command(cmd, "传输已取消")
                    };
                    let _ = task_event_tx.send(SessionEvent::Sftp(result));
                    task_controls.lock().expect("transfer controls").remove(&id);
                    task_ctx.request_repaint();
                });
                continue;
            }
            cmd => {
                let list_generation = if let SftpCmd::List(path) = &cmd {
                    let generation = next_list_generation;
                    next_list_generation = next_list_generation.wrapping_add(1).max(1);
                    latest_list_generations
                        .lock()
                        .expect("latest list generations")
                        .insert(path.clone(), generation);
                    Some((path.clone(), generation))
                } else {
                    None
                };
                let task_handle = Arc::clone(&handle);
                let task_sftp = Arc::clone(&sftp);
                let task_event_tx = event_tx.clone();
                let task_ctx = ctx.clone();
                let task_list_generations = Arc::clone(&latest_list_generations);
                let slots = if is_mutating_sftp_command(&cmd) {
                    Arc::clone(&mutation_slots)
                } else {
                    Arc::clone(&read_operation_slots)
                };
                tokio::spawn(async move {
                    let Ok(_permit) = slots.acquire_owned().await else {
                        return;
                    };
                    let event = run_sftp_operation(task_handle, task_sftp, cmd).await;
                    if let Some((path, generation)) = list_generation {
                        let mut latest = task_list_generations
                            .lock()
                            .expect("latest list generations");
                        if latest.get(&path) != Some(&generation) {
                            return;
                        }
                        latest.remove(&path);
                    }
                    let _ = task_event_tx.send(SessionEvent::Sftp(event));
                    task_ctx.request_repaint();
                });
                continue;
            }
        }
    }
}

fn is_mutating_sftp_command(cmd: &SftpCmd) -> bool {
    matches!(
        cmd,
        SftpCmd::Mkdir(_)
            | SftpCmd::CreateFile(_)
            | SftpCmd::Remove { .. }
            | SftpCmd::RemoveRecursive { .. }
            | SftpCmd::Rename { .. }
            | SftpCmd::SetPermissions { .. }
            | SftpCmd::WriteFile { .. }
    )
}

async fn run_sftp_operation(
    handle: Arc<client::Handle<ClientHandler>>,
    sftp: Arc<SftpSession>,
    cmd: SftpCmd,
) -> SftpEvent {
    match cmd {
        SftpCmd::List(path) => {
            let result = list_dir_resolved(&sftp, &path).await;
            SftpEvent::Listed { path, result }
        }
        SftpCmd::Mkdir(path) => SftpEvent::OpDone {
            id: None,
            op: "mkdir",
            result: sftp
                .create_dir(path)
                .await
                .map_err(|error| error.to_string()),
        },
        SftpCmd::CreateFile(path) => {
            let result = match sftp
                .open_with_flags(
                    &path,
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                )
                .await
            {
                Ok(mut file) => file.shutdown().await.map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            SftpEvent::OpDone {
                id: None,
                op: "create_file",
                result,
            }
        }
        SftpCmd::Remove { path, is_dir } => SftpEvent::OpDone {
            id: None,
            op: "remove",
            result: if is_dir {
                sftp.remove_dir(path)
                    .await
                    .map_err(|error| error.to_string())
            } else {
                sftp.remove_file(path)
                    .await
                    .map_err(|error| error.to_string())
            },
        },
        SftpCmd::RemoveRecursive { path } => SftpEvent::OpDone {
            id: None,
            op: "remove",
            result: remove_recursive(&sftp, &path).await,
        },
        SftpCmd::Rename { from, to } => SftpEvent::OpDone {
            id: None,
            op: "rename",
            result: sftp
                .rename(from, to)
                .await
                .map_err(|error| error.to_string()),
        },
        SftpCmd::SetPermissions { path, mode } => SftpEvent::OpDone {
            id: None,
            op: "chmod",
            result: set_remote_perms(&sftp, &path, mode).await,
        },
        SftpCmd::ReadFile {
            id,
            path,
            max_bytes,
        } => {
            let result = read_remote_capped(Arc::clone(&sftp), &path, max_bytes).await;
            SftpEvent::FileContent { id, path, result }
        }
        SftpCmd::WriteFile {
            id,
            path,
            data,
            expected_sha256,
            privilege,
        } => {
            let result = match privilege {
                WritePrivilege::Normal => {
                    write_remote_atomic(&handle, &sftp, id, &path, &data, expected_sha256).await
                }
                WritePrivilege::Elevated { password } => {
                    write_remote_elevated(
                        &handle,
                        &sftp,
                        id,
                        &path,
                        &data,
                        expected_sha256,
                        password.as_ref().map(|password| password.as_str()),
                    )
                    .await
                }
            };
            SftpEvent::FileWritten { id, result }
        }
        SftpCmd::DetectKind { id, path } => {
            let command = format!("file --mime-type -b -- {}", shell_quote(&path));
            let result = run_exec(&handle, &command)
                .await
                .map(|output| output.trim().to_string())
                .map_err(|error| error.to_string())
                .and_then(|mime| {
                    if mime.is_empty() {
                        Err("远端没有 file 命令或输出为空".to_string())
                    } else {
                        Ok(mime)
                    }
                });
            SftpEvent::KindDetected { id, path, result }
        }
        SftpCmd::Download { .. }
        | SftpCmd::DownloadDir { .. }
        | SftpCmd::UploadDir { .. }
        | SftpCmd::Upload { .. }
        | SftpCmd::CancelTransfer { .. }
        | SftpCmd::PauseTransfer { .. }
        | SftpCmd::ResumeTransfer { .. } => unreachable!("command was routed before operation"),
    }
}

/// shell 单引号转义(路径嵌入 exec 命令行时使用)
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// 读取远程文件前 max_bytes 字节到内存(供在线预览/编辑)
async fn read_remote_capped(
    sftp: Arc<SftpSession>,
    path: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let mut file = sftp
        .open(path)
        .await
        .map_err(|e| format!("无法打开 {path}: {e}"))?;
    // 小文件和 8 KiB 类型探测保持单请求，不额外支付 metadata 往返。只有首个
    // 256 KiB 已读满时才查询实际大小并启用并行分块。
    const INITIAL_READ: usize = 256 * 1024;
    const PARALLEL_CHUNK: u64 = 2 * 1024 * 1024;
    const PARALLEL_READS: usize = 8;
    let initial_len = max_bytes.min(INITIAL_READ as u64) as usize;
    let mut initial = vec![0u8; initial_len];
    let mut initial_read = 0usize;
    while initial_read < initial_len {
        let count = file
            .read(&mut initial[initial_read..])
            .await
            .map_err(|e| format!("读取 {path} 失败: {e}"))?;
        if count == 0 {
            break;
        }
        initial_read += count;
    }
    initial.truncate(initial_read);
    if initial_read < initial_len || initial_read as u64 >= max_bytes {
        return Ok(initial);
    }

    let total = match sftp.metadata(path).await {
        Ok(metadata) => metadata.size.unwrap_or(max_bytes).min(max_bytes),
        Err(_) => {
            // 某些精简 SFTP 服务允许读取但不允许 stat，退回原有顺序读取路径。
            let mut out = initial;
            let mut buffer = vec![0u8; INITIAL_READ];
            while (out.len() as u64) < max_bytes {
                let want = (max_bytes - out.len() as u64).min(buffer.len() as u64) as usize;
                let count = file
                    .read(&mut buffer[..want])
                    .await
                    .map_err(|e| format!("读取 {path} 失败: {e}"))?;
                if count == 0 {
                    break;
                }
                out.extend_from_slice(&buffer[..count]);
            }
            return Ok(out);
        }
    };
    drop(file);
    if total <= initial_read as u64 {
        initial.truncate(total as usize);
        return Ok(initial);
    }

    let mut tasks = tokio::task::JoinSet::new();
    let mut next_offset = initial_read as u64;
    for _ in 0..PARALLEL_READS {
        if next_offset >= total {
            break;
        }
        let len = (total - next_offset).min(PARALLEL_CHUNK) as usize;
        tasks.spawn(read_remote_chunk(
            Arc::clone(&sftp),
            path.to_string(),
            next_offset,
            len,
            None,
        ));
        next_offset += len as u64;
    }
    let mut ready = std::collections::BTreeMap::new();
    let mut append_offset = initial_read as u64;
    while let Some(result) = tasks.join_next().await {
        let (offset, data) = result.map_err(|e| format!("并行读取任务失败: {e}"))??;
        ready.insert(offset, data);
        while let Some(data) = ready.remove(&append_offset) {
            append_offset += data.len() as u64;
            initial.extend_from_slice(&data);
        }
        if next_offset < total {
            let len = (total - next_offset).min(PARALLEL_CHUNK) as usize;
            tasks.spawn(read_remote_chunk(
                Arc::clone(&sftp),
                path.to_string(),
                next_offset,
                len,
                None,
            ));
            next_offset += len as u64;
        }
    }
    Ok(initial)
}

const MAX_SAVE_VERIFY_BYTES: u64 = 32 * 1024 * 1024;

/// 普通编辑器保存：冲突检测、唯一临时文件、fsync、元数据保留和兼容替换。
async fn write_remote_atomic(
    handle: &client::Handle<ClientHandler>,
    sftp: &Arc<SftpSession>,
    id: u64,
    path: &str,
    data: &[u8],
    expected_sha256: Option<[u8; 32]>,
) -> WriteResult {
    let target = canonical_remote_file(sftp, path).await?;
    verify_remote_digest(handle, sftp, &target, expected_sha256).await?;
    let original = sftp.metadata(&target).await.map_err(|error| {
        map_sftp_failure(
            WriteFailureKind::Other,
            format!("读取 {target} 元数据失败"),
            error,
        )
    })?;
    if !original.file_type().is_file() {
        return Err(WriteFailure::new(
            WriteFailureKind::Unsupported,
            format!("{path} 不是普通文件，已拒绝替换"),
        ));
    }

    let (partial, mut file) = create_remote_temp(sftp, &target, id, "part").await?;
    let write_result = async {
        file.set_metadata(remote_mode_attributes(0o600))
            .await
            .map_err(|error| {
                map_sftp_failure(
                    WriteFailureKind::PermissionDenied,
                    format!("无法保护临时文件 {partial} 权限"),
                    error,
                )
            })?;
        file.write_all(data).await.map_err(|error| {
            WriteFailure::new(
                WriteFailureKind::Other,
                format!("写入临时文件 {partial} 失败: {error}"),
            )
        })?;
        file.sync_all().await.map_err(|error| {
            WriteFailure::new(
                WriteFailureKind::Other,
                format!("同步临时文件 {partial} 失败: {error}"),
            )
        })?;

        let temp_metadata = file.metadata().await.map_err(|error| {
            map_sftp_failure(
                WriteFailureKind::Other,
                format!("读取临时文件 {partial} 元数据失败"),
                error,
            )
        })?;
        let preserve = preserved_remote_attributes(&temp_metadata, &original);
        file.set_metadata(preserve).await.map_err(|error| {
            map_sftp_failure(
                WriteFailureKind::PermissionDenied,
                format!("无法保留 {target} 的所有者或权限"),
                error,
            )
        })?;
        file.sync_all().await.map_err(|error| {
            WriteFailure::new(
                WriteFailureKind::Other,
                format!("同步临时文件元数据失败: {error}"),
            )
        })?;
        file.shutdown().await.map_err(|error| {
            WriteFailure::new(
                WriteFailureKind::Other,
                format!("关闭临时文件 {partial} 失败: {error}"),
            )
        })?;
        Ok(())
    }
    .await;
    if let Err(error) = write_result {
        let _ = sftp.remove_file(&partial).await;
        return Err(error);
    }

    // GNU coreutils 可用时额外复制 ACL、xattr 和安全上下文；不支持该扩展的
    // Unix/Windows SFTP 主机仍保留上面已经设置的 owner/group/mode。
    let preserve_extended = format!(
        "cp --attributes-only --preserve=all -- {} {} >/dev/null 2>&1 && touch -- {}",
        shell_quote(&target),
        shell_quote(&partial),
        shell_quote(&partial)
    );
    let _ = run_exec_capture(handle, &preserve_extended, None).await;
    let wanted_digest: [u8; 32] = Sha256::digest(data).into();
    if verify_remote_digest(handle, sftp, &partial, Some(wanted_digest))
        .await
        .is_err()
    {
        let _ = sftp.remove_file(&partial).await;
        return Err(WriteFailure::new(
            WriteFailureKind::Other,
            "安全临时文件摘要校验失败，原文件未修改",
        ));
    }

    // 缩短“校验到替换”的竞态窗口；发现外部修改时保留用户编辑内容但不覆盖远端。
    if let Err(error) = verify_remote_digest(handle, sftp, &target, expected_sha256).await {
        let _ = sftp.remove_file(&partial).await;
        return Err(error);
    }

    // 有些 SFTP v3 服务允许覆盖式 rename，优先使用它保持真正的原子替换。
    if sftp.rename(&partial, &target).await.is_ok() {
        return verify_remote_digest(handle, sftp, &target, Some(wanted_digest)).await;
    }

    // 标准 SFTP v3 允许服务端拒绝覆盖已有目标。Unix 主机上用 mv(2) 语义补齐；
    // 命令和路径均为固定结构/单引号转义，不包含用户可执行片段。
    let mv_command = format!(
        "mv -f -- {} {}",
        shell_quote(&partial),
        shell_quote(&target)
    );
    if let Ok(output) = run_exec_capture(handle, &mv_command, None).await
        && output.status == Some(0)
    {
        return verify_remote_digest(handle, sftp, &target, Some(wanted_digest)).await;
    }
    // 少数服务器可能已经完成操作却返回非零；以实际内容为准。若失败的 rename
    // 异常移除了目标，则优先把完整临时文件恢复到正式路径，不进入备份流程。
    if verify_remote_digest(handle, sftp, &target, Some(wanted_digest))
        .await
        .is_ok()
    {
        let _ = sftp.remove_file(&partial).await;
        return Ok(());
    }
    match sftp.metadata(&target).await {
        Err(error) if is_sftp_status(&error, russh_sftp::protocol::StatusCode::NoSuchFile) => {
            return match sftp.rename(&partial, &target).await {
                Ok(()) => verify_remote_digest(handle, sftp, &target, Some(wanted_digest)).await,
                Err(error) => Err(WriteFailure::new(
                    WriteFailureKind::Other,
                    format!(
                        "服务器替换异常且目标文件暂时不存在；新内容保留在 {partial}，请立即检查: {error}"
                    ),
                )),
            };
        }
        Err(error) => {
            return Err(WriteFailure::new(
                WriteFailureKind::Other,
                format!("无法确认 {target} 的替换状态；新内容保留在 {partial}: {error}"),
            ));
        }
        Ok(_) => {}
    }

    // 非 Unix 或没有 mv 的服务端最后使用 SFTP 备份-替换-回滚兼容流程。
    let backup = unique_remote_sibling(&target, id, "backup");
    if let Err(error) = sftp.rename(&target, &backup).await {
        let _ = sftp.remove_file(&partial).await;
        return Err(map_sftp_failure(
            WriteFailureKind::PermissionDenied,
            format!("无法替换 {path}；当前账号没有目标目录写权限"),
            error,
        ));
    }
    match sftp.rename(&partial, &target).await {
        Ok(()) => {
            let _ = sftp.remove_file(&backup).await;
            verify_remote_digest(handle, sftp, &target, Some(wanted_digest)).await
        }
        Err(error) => {
            let rollback = sftp.rename(&backup, &target).await;
            if rollback.is_ok() {
                let _ = sftp.remove_file(&partial).await;
            }
            Err(WriteFailure::new(
                WriteFailureKind::Other,
                match rollback {
                    Ok(()) => format!("替换 {path} 失败，原文件已恢复: {error}"),
                    Err(rollback_error) => format!(
                        "替换 {path} 失败且自动恢复失败；原文件保留在 {backup}，新内容保留在 {partial}，请立即检查: {error}; {rollback_error}"
                    ),
                },
            ))
        }
    }
}

/// 权限不足时的管理员保存：先上传到用户私有临时文件，再由固定 sudo 脚本在目标
/// 目录复制原元数据、校验摘要并原子替换。sudo 密码只走 stdin。
async fn write_remote_elevated(
    handle: &client::Handle<ClientHandler>,
    sftp: &Arc<SftpSession>,
    id: u64,
    path: &str,
    data: &[u8],
    expected_sha256: Option<[u8; 32]>,
    password: Option<&str>,
) -> WriteResult {
    if password.is_some_and(|password| password.chars().any(|ch| ch == '\r' || ch == '\n')) {
        return Err(WriteFailure::new(
            WriteFailureKind::Authentication,
            "管理员密码不能包含换行符",
        ));
    }
    let target = canonical_remote_file(sftp, path).await?;
    if !target.starts_with('/') {
        return Err(WriteFailure::new(
            WriteFailureKind::Unsupported,
            "管理员保存目前仅支持 Unix/Linux SSH 主机的绝对路径",
        ));
    }
    verify_remote_digest(handle, sftp, &target, expected_sha256).await?;
    let original = sftp.metadata(&target).await.map_err(|error| {
        map_sftp_failure(
            WriteFailureKind::Other,
            format!("读取 {target} 元数据失败"),
            error,
        )
    })?;
    if !original.file_type().is_file() {
        return Err(WriteFailure::new(
            WriteFailureKind::Unsupported,
            format!("{path} 不是普通文件，已拒绝管理员替换"),
        ));
    }

    let home = sftp.canonicalize(".").await.map_err(|error| {
        map_sftp_failure(WriteFailureKind::Other, "无法确定远端用户目录", error)
    })?;
    let staged_base = super::files::join_path(&home, ".rshell-save-stage");
    let (staged, mut staged_file) = create_remote_temp(sftp, &staged_base, id, "upload").await?;
    let stage_result = async {
        staged_file
            .set_metadata(remote_mode_attributes(0o600))
            .await
            .map_err(|error| {
                map_sftp_failure(
                    WriteFailureKind::Other,
                    "无法保护管理员保存临时文件权限",
                    error,
                )
            })?;
        staged_file.write_all(data).await.map_err(|error| {
            WriteFailure::new(
                WriteFailureKind::Other,
                format!("上传管理员保存临时文件失败: {error}"),
            )
        })?;
        staged_file.sync_all().await.map_err(|error| {
            WriteFailure::new(
                WriteFailureKind::Other,
                format!("同步管理员保存临时文件失败: {error}"),
            )
        })?;
        staged_file.shutdown().await.map_err(|error| {
            WriteFailure::new(
                WriteFailureKind::Other,
                format!("关闭管理员保存临时文件失败: {error}"),
            )
        })?;
        Ok(())
    }
    .await;
    if let Err(error) = stage_result {
        let _ = sftp.remove_file(&staged).await;
        return Err(error);
    }

    // 在进入 sudo 前再检查一次；sudo 脚本内部还会做第三次无权限窗口校验。
    if let Err(error) = verify_remote_digest(handle, sftp, &target, expected_sha256).await {
        let _ = sftp.remove_file(&staged).await;
        return Err(error);
    }

    let expected = expected_sha256
        .map(hex_digest)
        .unwrap_or_else(|| "-".into());
    let new_digest = hex_digest(Sha256::digest(data).into());
    let mode = original
        .permissions
        .map(|mode| format!("{:04o}", mode & 0o7777))
        .unwrap_or_else(|| "-".into());
    let owner = match (original.uid, original.gid) {
        (Some(uid), Some(gid)) => format!("{uid}:{gid}"),
        _ => "-".into(),
    };
    let privileged_temp = unique_remote_sibling(&target, id, "root-part");
    let command = privileged_save_command(
        password.is_some(),
        &target,
        &staged,
        &expected,
        &new_digest,
        &mode,
        &owner,
        &privileged_temp,
    );
    let mut stdin = password.map(|password| {
        let mut bytes = password.as_bytes().to_vec();
        bytes.push(b'\n');
        bytes
    });
    let output = run_exec_capture(handle, &command, stdin.as_deref()).await;
    if let Some(stdin) = stdin.as_mut() {
        stdin.zeroize();
    }
    let _ = sftp.remove_file(&staged).await;
    let output = output.map_err(|message| WriteFailure::new(WriteFailureKind::Other, message))?;
    if output.status == Some(0) {
        return verify_remote_digest(handle, sftp, &target, Some(Sha256::digest(data).into()))
            .await;
    }
    Err(classify_privileged_failure(output))
}

const PRIVILEGED_SAVE_SCRIPT: &str = r#"set -eu
target=$1
stage=$2
expected=$3
wanted=$4
mode=$5
owner=$6
tmp=$7
hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        value=$(sha256sum "$1") || return 1
        printf '%s\n' "${value%% *}"
    elif command -v shasum >/dev/null 2>&1; then
        value=$(shasum -a 256 "$1") || return 1
        printf '%s\n' "${value%% *}"
    elif command -v sha256 >/dev/null 2>&1; then
        sha256 -q "$1"
    else
        return 1
    fi
}
[ -f "$target" ] && [ ! -L "$target" ] || exit 70
current=$(hash_file "$target") || exit 71
[ "$expected" = "-" ] || [ "$current" = "$expected" ] || exit 72
uploaded=$(hash_file "$stage") || exit 71
[ "$uploaded" = "$wanted" ] || exit 73
umask 077
(set -C; : > "$tmp") 2>/dev/null || exit 74
cleanup() { rm -f "$tmp"; }
trap cleanup EXIT HUP INT TERM
if cp --preserve=all -- "$target" "$tmp" 2>/dev/null; then
    :
elif cp -p "$target" "$tmp"; then
    :
else
    exit 75
fi
cat "$stage" > "$tmp"
verified=$(hash_file "$tmp") || exit 71
[ "$verified" = "$wanted" ] || exit 76
if cp --attributes-only --preserve=all -- "$target" "$tmp" 2>/dev/null; then
    touch "$tmp"
else
    [ "$owner" = "-" ] || chown "$owner" "$tmp"
    [ "$mode" = "-" ] || chmod "$mode" "$tmp"
fi
verified=$(hash_file "$tmp") || exit 71
[ "$verified" = "$wanted" ] || exit 76
case "$target" in
    /etc/sudoers|/etc/sudoers.d/*|/private/etc/sudoers|/private/etc/sudoers.d/*)
        command -v visudo >/dev/null 2>&1 || exit 77
        visudo -cf "$tmp" >/dev/null || exit 78
        ;;
    /etc/ssh/sshd_config|/private/etc/ssh/sshd_config)
        command -v sshd >/dev/null 2>&1 || exit 79
        sshd -t -f "$tmp" >/dev/null || exit 80
        ;;
    /etc/nginx/nginx.conf|/private/etc/nginx/nginx.conf)
        command -v nginx >/dev/null 2>&1 || exit 81
        nginx -t -c "$tmp" >/dev/null || exit 82
        ;;
esac
sync
mv -f -- "$tmp" "$target"
trap - EXIT HUP INT TERM
sync || :
rm -f "$stage" || :
exit 0
"#;

#[allow(clippy::too_many_arguments)]
fn privileged_save_command(
    has_password: bool,
    target: &str,
    staged: &str,
    expected: &str,
    wanted: &str,
    mode: &str,
    owner: &str,
    temp: &str,
) -> String {
    let sudo_flags = if has_password { "-S -p ''" } else { "-n" };
    format!(
        "sudo {sudo_flags} -- /bin/sh -c {} rshell-save {} {} {} {} {} {} {}",
        shell_quote(PRIVILEGED_SAVE_SCRIPT),
        shell_quote(target),
        shell_quote(staged),
        shell_quote(expected),
        shell_quote(wanted),
        shell_quote(mode),
        shell_quote(owner),
        shell_quote(temp),
    )
}

fn classify_privileged_failure(output: ExecOutput) -> WriteFailure {
    let detail = [output.stderr.trim(), output.stdout.trim()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    let detail = if detail.is_empty() {
        format!("远端命令退出码 {}", output.status.unwrap_or(u32::MAX))
    } else {
        detail
    };
    match output.status {
        Some(72) => WriteFailure::new(
            WriteFailureKind::Conflict,
            "管理员保存前检测到远端文件已被其他程序修改，未覆盖",
        ),
        Some(77) => WriteFailure::new(
            WriteFailureKind::Validation,
            "远端缺少 visudo，无法安全校验 sudoers 文件，已拒绝覆盖",
        ),
        Some(78) => WriteFailure::new(
            WriteFailureKind::Validation,
            format!("sudoers 语法校验失败，原文件未修改: {detail}"),
        ),
        Some(79) => WriteFailure::new(
            WriteFailureKind::Validation,
            "远端缺少 sshd，无法安全校验 sshd_config，已拒绝覆盖",
        ),
        Some(80) => WriteFailure::new(
            WriteFailureKind::Validation,
            format!("sshd_config 语法校验失败，原文件未修改: {detail}"),
        ),
        Some(81) => WriteFailure::new(
            WriteFailureKind::Validation,
            "远端缺少 nginx，无法安全校验 nginx.conf，已拒绝覆盖",
        ),
        Some(82) => WriteFailure::new(
            WriteFailureKind::Validation,
            format!("nginx.conf 语法校验失败，原文件未修改: {detail}"),
        ),
        Some(71 | 73 | 74 | 75 | 76) => WriteFailure::new(
            WriteFailureKind::Unsupported,
            format!("远端缺少安全原子保存所需能力，原文件未修改: {detail}"),
        ),
        _ => {
            let lower = detail.to_ascii_lowercase();
            let policy_denied = lower.contains("not in the sudoers")
                || lower.contains("not allowed to execute")
                || lower.contains("may not run sudo")
                || lower.contains("requiretty")
                || (lower.contains("sudo") && lower.contains("not found"));
            let authentication = !policy_denied
                && (lower.contains("password")
                    || lower.contains("authentication")
                    || lower.contains("sudo:")
                    || lower.contains("sudoers"));
            WriteFailure::new(
                if policy_denied {
                    WriteFailureKind::Unsupported
                } else if authentication {
                    WriteFailureKind::Authentication
                } else {
                    WriteFailureKind::Other
                },
                format!("管理员保存失败，原文件未修改: {detail}"),
            )
        }
    }
}

async fn canonical_remote_file(sftp: &SftpSession, path: &str) -> Result<String, WriteFailure> {
    sftp.canonicalize(path).await.map_err(|error| {
        map_sftp_failure(
            WriteFailureKind::Other,
            format!("无法解析目标文件 {path}"),
            error,
        )
    })
}

async fn verify_remote_digest(
    handle: &client::Handle<ClientHandler>,
    sftp: &Arc<SftpSession>,
    path: &str,
    expected: Option<[u8; 32]>,
) -> WriteResult {
    let Some(expected) = expected else {
        return Ok(());
    };
    let metadata = sftp.metadata(path).await.map_err(|error| {
        map_sftp_failure(
            WriteFailureKind::Other,
            format!("保存前无法读取 {path} 元数据"),
            error,
        )
    })?;
    if metadata.size.unwrap_or(MAX_SAVE_VERIFY_BYTES + 1) > MAX_SAVE_VERIFY_BYTES {
        return Err(WriteFailure::new(
            WriteFailureKind::Conflict,
            format!("{path} 已变为过大的文件，已拒绝覆盖"),
        ));
    }
    let size = metadata.size.unwrap_or(0);
    // 大文件优先在服务器本地计算摘要，只在网络上传回 64 个十六进制字符。
    // Windows/精简 Unix 主机没有受支持的哈希命令时自动回退 SFTP 读取。
    const REMOTE_HASH_THRESHOLD: u64 = 512 * 1024;
    if size >= REMOTE_HASH_THRESHOLD
        && let Some(actual) = remote_sha256(handle, path).await
    {
        return compare_remote_digest(path, actual, expected);
    }

    let mut file = sftp.open(path).await.map_err(|error| {
        map_sftp_failure(
            WriteFailureKind::Other,
            format!("保存前无法读取 {path}"),
            error,
        )
    })?;
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).await.map_err(|error| {
            WriteFailure::new(
                WriteFailureKind::Other,
                format!("保存前校验 {path} 失败: {error}"),
            )
        })?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_SAVE_VERIFY_BYTES {
            return Err(WriteFailure::new(
                WriteFailureKind::Conflict,
                format!("{path} 已变为过大的文件，已拒绝覆盖"),
            ));
        }
        digest.update(&buffer[..read]);
    }
    let actual: [u8; 32] = digest.finalize().into();
    compare_remote_digest(path, actual, expected)
}

fn compare_remote_digest(path: &str, actual: [u8; 32], expected: [u8; 32]) -> WriteResult {
    if actual == expected {
        Ok(())
    } else {
        Err(WriteFailure::new(
            WriteFailureKind::Conflict,
            format!("{path} 自打开或上次保存后已被其他程序修改，未覆盖"),
        ))
    }
}

async fn remote_sha256(handle: &client::Handle<ClientHandler>, path: &str) -> Option<[u8; 32]> {
    let quoted = shell_quote(path);
    let command = format!(
        "if command -v sha256sum >/dev/null 2>&1; then sha256sum -- {quoted}; \
         elif command -v shasum >/dev/null 2>&1; then shasum -a 256 -- {quoted}; \
         elif command -v sha256 >/dev/null 2>&1; then sha256 -q {quoted}; \
         else exit 127; fi"
    );
    let output = run_exec_capture(handle, &command, None).await.ok()?;
    if output.status != Some(0) {
        return None;
    }
    parse_hex_digest(output.stdout.split_whitespace().next()?)
}

fn parse_hex_digest(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

async fn create_remote_temp(
    sftp: &SftpSession,
    target: &str,
    id: u64,
    purpose: &str,
) -> Result<(String, russh_sftp::client::fs::File), WriteFailure> {
    let mut last_error = None;
    for _ in 0..3 {
        let path = unique_remote_sibling(target, id, purpose);
        match sftp
            .open_with_flags(
                &path,
                OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
            )
            .await
        {
            Ok(file) => return Ok((path, file)),
            Err(error) => last_error = Some(error),
        }
    }
    let error = last_error.expect("three remote temp attempts");
    Err(map_sftp_failure(
        WriteFailureKind::PermissionDenied,
        format!("无法在目标目录创建安全临时文件 {target}"),
        error,
    ))
}

fn unique_remote_sibling(target: &str, id: u64, purpose: &str) -> String {
    let parent = super::files::parent_path(target);
    let name = target
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("file");
    let mut nonce = [0u8; 8];
    rand::fill(&mut nonce);
    let nonce = u64::from_ne_bytes(nonce);
    super::files::join_path(
        &parent,
        &format!(".{name}.rshell-{purpose}-{id}-{nonce:016x}"),
    )
}

fn map_sftp_failure(
    fallback: WriteFailureKind,
    context: impl Into<String>,
    error: russh_sftp::client::error::Error,
) -> WriteFailure {
    let kind = match &error {
        russh_sftp::client::error::Error::Status(status)
            if status.status_code == russh_sftp::protocol::StatusCode::PermissionDenied =>
        {
            WriteFailureKind::PermissionDenied
        }
        _ => fallback,
    };
    WriteFailure::new(kind, format!("{}: {error}", context.into()))
}

fn is_sftp_status(
    error: &russh_sftp::client::error::Error,
    wanted: russh_sftp::protocol::StatusCode,
) -> bool {
    matches!(error, russh_sftp::client::error::Error::Status(status) if status.status_code == wanted)
}

fn hex_digest(digest: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn transfer_command_id(cmd: &SftpCmd) -> u64 {
    match cmd {
        SftpCmd::Download { id, .. }
        | SftpCmd::DownloadDir { id, .. }
        | SftpCmd::UploadDir { id, .. }
        | SftpCmd::Upload { id, .. } => *id,
        _ => unreachable!("not a transfer command"),
    }
}

async fn run_remote_transfer(
    sftp: Arc<SftpSession>,
    cmd: SftpCmd,
    control: Arc<TransferControl>,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> SftpEvent {
    let (id, op, result) = match cmd {
        SftpCmd::Download { id, remote, local } => (
            id,
            "download",
            download(
                Arc::clone(&sftp),
                id,
                &remote,
                &local,
                Arc::clone(&control),
                event_tx,
                ctx,
            )
            .await,
        ),
        SftpCmd::DownloadDir { id, remote, local } => (
            id,
            "download_dir",
            download_dir(
                Arc::clone(&sftp),
                id,
                &remote,
                &local,
                Arc::clone(&control),
                event_tx,
                ctx,
            )
            .await,
        ),
        SftpCmd::UploadDir { id, local, remote } => (
            id,
            "upload_dir",
            upload_dir(
                Arc::clone(&sftp),
                id,
                &local,
                &remote,
                Arc::clone(&control),
                event_tx,
                ctx,
            )
            .await,
        ),
        SftpCmd::Upload { id, local, remote } => (
            id,
            "upload",
            upload(&sftp, id, &local, &remote, &control, event_tx, ctx).await,
        ),
        _ => unreachable!("not a transfer command"),
    };
    SftpEvent::OpDone {
        id: Some(id),
        op,
        result,
    }
}

fn failed_sftp_command(cmd: SftpCmd, message: &str) -> SftpEvent {
    match cmd {
        SftpCmd::List(path) => SftpEvent::Listed {
            path,
            result: Err(message.to_string()),
        },
        SftpCmd::Mkdir(_) => SftpEvent::OpDone {
            id: None,
            op: "mkdir",
            result: Err(message.to_string()),
        },
        SftpCmd::CreateFile(_) => SftpEvent::OpDone {
            id: None,
            op: "create_file",
            result: Err(message.to_string()),
        },
        SftpCmd::Remove { .. } | SftpCmd::RemoveRecursive { .. } => SftpEvent::OpDone {
            id: None,
            op: "remove",
            result: Err(message.to_string()),
        },
        SftpCmd::Rename { .. } => SftpEvent::OpDone {
            id: None,
            op: "rename",
            result: Err(message.to_string()),
        },
        SftpCmd::Download { id, .. } => SftpEvent::OpDone {
            id: Some(id),
            op: "download",
            result: Err(message.to_string()),
        },
        SftpCmd::Upload { id, .. } => SftpEvent::OpDone {
            id: Some(id),
            op: "upload",
            result: Err(message.to_string()),
        },
        SftpCmd::DownloadDir { id, .. } => SftpEvent::OpDone {
            id: Some(id),
            op: "download_dir",
            result: Err(message.to_string()),
        },
        SftpCmd::UploadDir { id, .. } => SftpEvent::OpDone {
            id: Some(id),
            op: "upload_dir",
            result: Err(message.to_string()),
        },
        SftpCmd::SetPermissions { .. } => SftpEvent::OpDone {
            id: None,
            op: "chmod",
            result: Err(message.to_string()),
        },
        SftpCmd::ReadFile { id, path, .. } => SftpEvent::FileContent {
            id,
            path,
            result: Err(message.to_string()),
        },
        SftpCmd::WriteFile { id, .. } => SftpEvent::FileWritten {
            id,
            result: Err(WriteFailure::new(WriteFailureKind::Other, message)),
        },
        SftpCmd::DetectKind { id, path } => SftpEvent::KindDetected {
            id,
            path,
            result: Err(message.to_string()),
        },
        SftpCmd::CancelTransfer { .. }
        | SftpCmd::PauseTransfer { .. }
        | SftpCmd::ResumeTransfer { .. } => SftpEvent::SetupFailed(message.to_string()),
    }
}

/// 递归删除远端目录:深度优先删子项,再删自身;
/// 符号链接(含指向目录的)按链接删除,不进入链接内部
async fn remove_recursive(sftp: &SftpSession, path: &str) -> Result<(), String> {
    let meta = sftp
        .symlink_metadata(path)
        .await
        .map_err(|e| e.to_string())?;
    // 符号链接直接 unlink(即便指向目录也不递归进链接内部)
    if meta.file_type().is_symlink() {
        return sftp.remove_file(path).await.map_err(|e| e.to_string());
    }
    if !meta.file_type().is_dir() {
        return sftp.remove_file(path).await.map_err(|e| e.to_string());
    }
    let rd = sftp.read_dir(path).await.map_err(|e| e.to_string())?;
    for e in rd {
        let name = e.file_name();
        super::files::validate_sftp_name(&name)
            .map_err(|error| format!("远端目录 {path} 返回不安全条目: {error}"))?;
        let child = super::files::join_path(path, &name);
        Box::pin(remove_recursive(sftp, &child)).await?;
    }
    sftp.remove_dir(path).await.map_err(|e| e.to_string())
}

const DIRECTORY_FILE_CONCURRENCY: usize = 8;

struct DownloadDirJob {
    remote: String,
    local: std::path::PathBuf,
    expected: u64,
    source_tag: String,
}

struct UploadDirJob {
    local: std::path::PathBuf,
    remote: String,
    expected: u64,
    source_tag: String,
}

struct DirectoryProgress {
    id: u64,
    op: &'static str,
    path: String,
    done: std::sync::atomic::AtomicU64,
    total: u64,
    last: Mutex<Instant>,
    event_tx: Sender<SessionEvent>,
    ctx: egui::Context,
}

impl DirectoryProgress {
    fn new(
        id: u64,
        op: &'static str,
        path: &str,
        total: u64,
        event_tx: &Sender<SessionEvent>,
        ctx: &egui::Context,
    ) -> Self {
        Self {
            id,
            op,
            path: path.to_string(),
            done: std::sync::atomic::AtomicU64::new(0),
            total,
            last: Mutex::new(Instant::now() - Duration::from_millis(100)),
            event_tx: event_tx.clone(),
            ctx: ctx.clone(),
        }
    }

    fn add(&self, bytes: u64) {
        self.done
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        self.report(false);
    }

    fn report(&self, force: bool) {
        let mut last = self.last.lock().expect("directory progress clock");
        if !force && last.elapsed() < Duration::from_millis(100) {
            return;
        }
        let _ = self.event_tx.send(SessionEvent::Sftp(SftpEvent::Progress {
            id: self.id,
            op: self.op,
            path: self.path.clone(),
            done: self.done.load(std::sync::atomic::Ordering::Relaxed),
            total: self.total,
        }));
        *last = Instant::now();
        self.ctx.request_repaint();
    }
}

fn local_metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

async fn ensure_safe_download_root(path: &std::path::Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("下载目录不能为空".to_string());
    }
    // 逐级检查和创建，避免 create_dir_all 悄悄跟随任意一个中间
    // symlink/reparse point，把后续服务器文件写到用户选择目录之外。
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(format!("下载目录不能包含 ..: {}", path.display()));
        }
        current.push(component.as_os_str());
        if matches!(
            component,
            std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::CurDir
        ) {
            continue;
        }
        match tokio::fs::symlink_metadata(&current).await {
            Ok(metadata) => validate_download_directory(&current, &metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&current)
                    .await
                    .map_err(|error| format!("创建下载目录 {} 失败: {error}", current.display()))?;
                let metadata = tokio::fs::symlink_metadata(&current)
                    .await
                    .map_err(|error| format!("验证下载目录 {} 失败: {error}", current.display()))?;
                validate_download_directory(&current, &metadata)?;
            }
            Err(error) => {
                return Err(format!("检查下载目录 {} 失败: {error}", current.display()));
            }
        }
    }
    Ok(())
}

async fn ensure_safe_download_child(path: &std::path::Path) -> Result<(), String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => validate_download_directory(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // 父目录已经逐层验证，不使用 create_dir_all 跟随未检查的中间链接。
            tokio::fs::create_dir(path)
                .await
                .map_err(|error| format!("创建下载子目录 {} 失败: {error}", path.display()))?;
            let metadata = tokio::fs::symlink_metadata(path)
                .await
                .map_err(|error| format!("验证下载子目录 {} 失败: {error}", path.display()))?;
            validate_download_directory(path, &metadata)
        }
        Err(error) => Err(format!("检查下载子目录 {} 失败: {error}", path.display())),
    }
}

fn validate_download_directory(
    path: &std::path::Path,
    metadata: &std::fs::Metadata,
) -> Result<(), String> {
    if local_metadata_is_link_or_reparse(metadata) {
        return Err(format!(
            "已拒绝跟随下载目录中的符号链接或 reparse point: {}",
            path.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!("下载路径已存在但不是目录: {}", path.display()));
    }
    Ok(())
}

async fn reject_local_link_if_exists(path: &std::path::Path) -> Result<(), String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if local_metadata_is_link_or_reparse(&metadata) => Err(format!(
            "已拒绝使用符号链接或 reparse point 作为下载临时文件: {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("检查下载临时文件 {} 失败: {error}", path.display())),
    }
}

/// 远程→本地目录：先收集稳定的文件快照，再以有界并发下载文件。
async fn download_dir(
    sftp: Arc<SftpSession>,
    id: u64,
    remote: &str,
    local: &str,
    control: Arc<TransferControl>,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    ensure_safe_download_root(std::path::Path::new(local)).await?;
    let mut jobs = Vec::new();
    Box::pin(collect_download_jobs(
        &sftp,
        remote,
        std::path::Path::new(local),
        &control,
        &mut jobs,
    ))
    .await?;
    let total = jobs
        .iter()
        .fold(0u64, |sum, job| sum.saturating_add(job.expected));
    let progress = Arc::new(DirectoryProgress::new(
        id,
        "download_dir",
        remote,
        total,
        event_tx,
        ctx,
    ));
    progress.report(true);
    run_download_jobs(sftp, jobs, control, Arc::clone(&progress)).await?;
    progress.report(true);
    Ok(())
}

async fn collect_download_jobs(
    sftp: &SftpSession,
    remote_dir: &str,
    local_dir: &std::path::Path,
    control: &TransferControl,
    jobs: &mut Vec<DownloadDirJob>,
) -> Result<(), String> {
    control.wait_async().await?;
    let entries = sftp
        .read_dir(remote_dir)
        .await
        .map_err(|error| format!("读取远程目录 {remote_dir} 失败: {error}"))?;
    for entry in entries {
        control.wait_async().await?;
        let name = entry.file_name();
        super::files::validate_sftp_name(&name)
            .map_err(|error| format!("远端目录 {remote_dir} 返回不安全条目: {error}"))?;
        let remote_path = super::files::join_path(remote_dir, &name);
        let local_path = local_dir.join(&name);
        if entry.file_type().is_symlink() {
            return Err(format!("目录传输暂不跟随符号链接，已停止于：{remote_path}"));
        }
        if entry.file_type().is_dir() {
            ensure_safe_download_child(&local_path).await?;
            Box::pin(collect_download_jobs(
                sftp,
                &remote_path,
                &local_path,
                control,
                jobs,
            ))
            .await?;
        } else {
            let mut metadata = entry.metadata();
            if metadata.size.is_none() {
                metadata = sftp
                    .metadata(&remote_path)
                    .await
                    .map_err(|error| format!("读取 {remote_path} 信息失败: {error}"))?;
            }
            let expected = metadata.size.unwrap_or(0);
            jobs.push(DownloadDirJob {
                remote: remote_path,
                local: local_path,
                expected,
                source_tag: remote_source_tag(expected, metadata.mtime),
            });
        }
    }
    Ok(())
}

async fn run_download_jobs(
    sftp: Arc<SftpSession>,
    jobs: Vec<DownloadDirJob>,
    control: Arc<TransferControl>,
    progress: Arc<DirectoryProgress>,
) -> Result<(), String> {
    let mut jobs = jobs.into_iter();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..DIRECTORY_FILE_CONCURRENCY {
        let Some(job) = jobs.next() else { break };
        tasks.spawn(download_directory_job(
            Arc::clone(&sftp),
            job,
            Arc::clone(&control),
            Arc::clone(&progress),
        ));
    }
    while let Some(result) = tasks.join_next().await {
        result.map_err(|error| format!("目录下载任务失败: {error}"))??;
        if let Some(job) = jobs.next() {
            tasks.spawn(download_directory_job(
                Arc::clone(&sftp),
                job,
                Arc::clone(&control),
                Arc::clone(&progress),
            ));
        }
    }
    Ok(())
}

async fn download_directory_job(
    sftp: Arc<SftpSession>,
    job: DownloadDirJob,
    control: Arc<TransferControl>,
    progress: Arc<DirectoryProgress>,
) -> Result<(), String> {
    let local = job.local.to_string_lossy();
    let partial = local_partial_path(&local);
    let resume = prepare_local_partial(&partial, &job.source_tag, job.expected).await?;
    let mut source = sftp
        .open(&job.remote)
        .await
        .map_err(|error| error.to_string())?;
    source
        .seek(std::io::SeekFrom::Start(resume))
        .await
        .map_err(|error| error.to_string())?;
    let mut target = open_local_download_partial(&partial, resume).await?;
    progress.add(resume);
    let mut file_done = resume;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        control.wait_async().await?;
        let count = source
            .read(&mut buffer)
            .await
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        target
            .write_all(&buffer[..count])
            .await
            .map_err(|error| format!("写入 {partial} 失败: {error}"))?;
        file_done += count as u64;
        progress.add(count as u64);
    }
    if file_done != job.expected {
        return Err(format!(
            "下载 {} 不完整：预期 {} 字节，实际 {} 字节",
            job.remote, job.expected, file_done
        ));
    }
    let final_metadata = sftp
        .metadata(&job.remote)
        .await
        .map_err(|error| format!("重新读取 {} 信息失败: {error}", job.remote))?;
    if remote_source_tag(final_metadata.size.unwrap_or(0), final_metadata.mtime) != job.source_tag {
        return Err(format!("下载期间远程源文件 {} 发生变化", job.remote));
    }
    control.wait_async().await?;
    target.flush().await.map_err(|error| error.to_string())?;
    target.sync_all().await.map_err(|error| error.to_string())?;
    drop(target);
    commit_local_partial(&partial, &local).await
}

/// 本地→远程目录：目录只创建一次，文件使用有界并发上传。
async fn upload_dir(
    sftp: Arc<SftpSession>,
    id: u64,
    local: &str,
    remote: &str,
    control: Arc<TransferControl>,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    ensure_remote_dir_all(&sftp, remote).await?;
    let mut jobs = Vec::new();
    Box::pin(collect_upload_jobs(
        &sftp,
        std::path::Path::new(local),
        remote,
        &control,
        &mut jobs,
    ))
    .await?;
    let total = jobs
        .iter()
        .fold(0u64, |sum, job| sum.saturating_add(job.expected));
    let progress = Arc::new(DirectoryProgress::new(
        id,
        "upload_dir",
        local,
        total,
        event_tx,
        ctx,
    ));
    progress.report(true);
    run_upload_jobs(sftp, jobs, control, Arc::clone(&progress)).await?;
    progress.report(true);
    Ok(())
}

async fn collect_upload_jobs(
    sftp: &SftpSession,
    local_dir: &std::path::Path,
    remote_dir: &str,
    control: &TransferControl,
    jobs: &mut Vec<UploadDirJob>,
) -> Result<(), String> {
    control.wait_async().await?;
    let mut entries = tokio::fs::read_dir(local_dir)
        .await
        .map_err(|error| format!("读取本地目录 {} 失败: {error}", local_dir.display()))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| format!("遍历本地目录 {} 失败: {error}", local_dir.display()))?
    {
        control.wait_async().await?;
        let local_path = entry.path();
        let file_type = entry
            .file_type()
            .await
            .map_err(|error| format!("读取 {} 类型失败: {error}", local_path.display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "目录传输暂不跟随符号链接，已停止于：{}",
                local_path.display()
            ));
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let remote_path = super::files::join_path(remote_dir, &name);
        let metadata = entry
            .metadata()
            .await
            .map_err(|error| format!("读取 {} 信息失败: {error}", local_path.display()))?;
        if metadata.is_dir() {
            ensure_remote_child_dir(sftp, &remote_path).await?;
            Box::pin(collect_upload_jobs(
                sftp,
                &local_path,
                &remote_path,
                control,
                jobs,
            ))
            .await?;
        } else {
            jobs.push(UploadDirJob {
                local: local_path,
                remote: remote_path,
                expected: metadata.len(),
                source_tag: local_source_tag(&metadata),
            });
        }
    }
    Ok(())
}

async fn ensure_remote_child_dir(sftp: &SftpSession, path: &str) -> Result<(), String> {
    match sftp.create_dir(path).await {
        Ok(()) => Ok(()),
        Err(create_error) => match sftp.metadata(path).await {
            Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
            _ => Err(format!("创建远程目录 {path} 失败：{create_error}")),
        },
    }
}

async fn run_upload_jobs(
    sftp: Arc<SftpSession>,
    jobs: Vec<UploadDirJob>,
    control: Arc<TransferControl>,
    progress: Arc<DirectoryProgress>,
) -> Result<(), String> {
    let mut jobs = jobs.into_iter();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..DIRECTORY_FILE_CONCURRENCY {
        let Some(job) = jobs.next() else { break };
        tasks.spawn(upload_directory_job(
            Arc::clone(&sftp),
            job,
            Arc::clone(&control),
            Arc::clone(&progress),
        ));
    }
    while let Some(result) = tasks.join_next().await {
        result.map_err(|error| format!("目录上传任务失败: {error}"))??;
        if let Some(job) = jobs.next() {
            tasks.spawn(upload_directory_job(
                Arc::clone(&sftp),
                job,
                Arc::clone(&control),
                Arc::clone(&progress),
            ));
        }
    }
    Ok(())
}

async fn upload_directory_job(
    sftp: Arc<SftpSession>,
    job: UploadDirJob,
    control: Arc<TransferControl>,
    progress: Arc<DirectoryProgress>,
) -> Result<(), String> {
    let local = job.local.to_string_lossy();
    let partial = remote_partial_path(&job.remote);
    let resume = prepare_remote_partial(&sftp, &partial, &job.source_tag, job.expected).await?;
    let mut source = tokio::fs::File::open(&job.local)
        .await
        .map_err(|error| format!("打开 {local} 失败: {error}"))?;
    source
        .seek(std::io::SeekFrom::Start(resume))
        .await
        .map_err(|error| error.to_string())?;
    let mut target = sftp
        .open_with_flags(&partial, OpenFlags::CREATE | OpenFlags::WRITE)
        .await
        .map_err(|error| format!("创建远程临时文件 {partial} 失败: {error}"))?;
    target
        .seek(std::io::SeekFrom::Start(resume))
        .await
        .map_err(|error| error.to_string())?;
    progress.add(resume);
    let mut file_done = resume;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        control.wait_async().await?;
        let count = source
            .read(&mut buffer)
            .await
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        target
            .write_all(&buffer[..count])
            .await
            .map_err(|error| format!("写入 {} 失败: {error}", job.remote))?;
        file_done += count as u64;
        progress.add(count as u64);
    }
    target.shutdown().await.map_err(|error| error.to_string())?;
    if file_done != job.expected {
        return Err(format!(
            "上传 {local} 不完整：预期 {} 字节，实际 {file_done} 字节",
            job.expected
        ));
    }
    let final_metadata = tokio::fs::metadata(&job.local)
        .await
        .map_err(|error| format!("重新读取 {local} 信息失败: {error}"))?;
    if local_source_tag(&final_metadata) != job.source_tag {
        return Err(format!("上传期间本地源文件 {local} 发生变化"));
    }
    control.wait_async().await?;
    let remote_size = sftp
        .metadata(&partial)
        .await
        .map_err(|error| error.to_string())?
        .size
        .unwrap_or(0);
    if remote_size != job.expected {
        return Err(format!(
            "远程文件 {} 大小校验失败：预期 {}，实际 {remote_size}",
            job.remote, job.expected
        ));
    }
    commit_remote_partial(&sftp, &partial, &job.remote).await
}

async fn ensure_remote_dir_all(sftp: &SftpSession, path: &str) -> Result<(), String> {
    let normalized = super::files::normalize_path(path);
    if normalized == "/" {
        return Ok(());
    }
    let mut current = String::new();
    for component in normalized.split('/').filter(|part| !part.is_empty()) {
        current.push('/');
        current.push_str(component);
        match sftp.metadata(&current).await {
            Ok(metadata) if metadata.file_type().is_dir() => continue,
            Ok(_) => return Err(format!("远程路径已存在但不是目录：{current}")),
            Err(_) => {
                if let Err(create_error) = sftp.create_dir(&current).await {
                    match sftp.metadata(&current).await {
                        Ok(metadata) if metadata.file_type().is_dir() => {}
                        _ => return Err(format!("创建远程目录 {current} 失败：{create_error}")),
                    }
                }
            }
        }
    }
    Ok(())
}

/// 设置远端权限:setstat 只动权限位
async fn set_remote_perms(sftp: &SftpSession, path: &str, mode: u32) -> Result<(), String> {
    let attrs = remote_mode_attributes(mode);
    sftp.set_metadata(path, attrs)
        .await
        .map_err(|e| e.to_string())
}

/// `FileAttributes::default()` contains concrete values, including `size = 0`.
/// A metadata-only SETSTAT/FSETSTAT must start from `empty()` so omitted fields
/// are not serialized and changing permissions cannot truncate the file.
fn remote_mode_attributes(mode: u32) -> russh_sftp::protocol::FileAttributes {
    russh_sftp::protocol::FileAttributes {
        permissions: Some(mode),
        ..russh_sftp::protocol::FileAttributes::empty()
    }
}

fn preserved_remote_attributes(
    current: &russh_sftp::protocol::FileAttributes,
    original: &russh_sftp::protocol::FileAttributes,
) -> russh_sftp::protocol::FileAttributes {
    russh_sftp::protocol::FileAttributes {
        uid: (current.uid != original.uid)
            .then_some(original.uid)
            .flatten(),
        gid: (current.gid != original.gid)
            .then_some(original.gid)
            .flatten(),
        permissions: original.permissions,
        ..russh_sftp::protocol::FileAttributes::empty()
    }
}

async fn open_sftp<H>(handle: &client::Handle<H>) -> Result<SftpSession, String>
where
    H: Handler,
{
    tokio::time::timeout(Duration::from_secs(30), async {
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|error| error.to_string())?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|error| error.to_string())?;
        SftpSession::new_with_config(
            channel.into_stream(),
            russh_sftp::client::Config {
                max_packet_len: 256 * 1024,
                max_concurrent_writes: 16,
                request_timeout_secs: 30,
            },
        )
        .await
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| "SFTP 初始化超时（30 秒）".to_string())?
}

/// 列目录,并把符号链接解析为目标的类型(/bin、/etc 这类链接目录按目录处理)
async fn list_dir_resolved(sftp: &Arc<SftpSession>, path: &str) -> Result<Vec<FileEntry>, String> {
    const SYMLINK_STAT_CONCURRENCY: usize = 16;
    let rd = sftp.read_dir(path).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut stats = tokio::task::JoinSet::new();
    for e in rd {
        let name = e.file_name();
        super::files::validate_sftp_name(&name)
            .map_err(|error| format!("远端目录 {path} 返回不安全条目: {error}"))?;
        let entry = dir_entry_to_file_entry(&e);
        let index = out.len();
        let is_symlink = entry.is_symlink;
        out.push(entry);
        if is_symlink {
            let task_sftp = Arc::clone(sftp);
            let entry_path = e.path();
            stats.spawn(async move { (index, task_sftp.metadata(entry_path).await) });
            if stats.len() >= SYMLINK_STAT_CONCURRENCY
                && let Some(Ok((resolved_index, result))) = stats.join_next().await
            {
                match result {
                    Ok(metadata) => apply_symlink_metadata(&mut out[resolved_index], metadata),
                    Err(_) => out[resolved_index].is_broken_symlink = true,
                }
            }
        }
    }
    while let Some(result) = stats.join_next().await {
        if let Ok((index, result)) = result {
            match result {
                Ok(metadata) => apply_symlink_metadata(&mut out[index], metadata),
                Err(_) => out[index].is_broken_symlink = true,
            }
        }
    }
    Ok(out)
}

fn apply_symlink_metadata(entry: &mut FileEntry, metadata: russh_sftp::client::fs::Metadata) {
    // stat 跟随链接；悬空链接保持 read_dir 返回的原样（非目录）。
    entry.is_dir = metadata.file_type().is_dir();
    entry.size = metadata.size.unwrap_or(0);
    entry.mtime = metadata
        .mtime
        .map(|time| time as i64)
        .unwrap_or(entry.mtime);
}

fn dir_entry_to_file_entry(e: &russh_sftp::client::fs::DirEntry) -> FileEntry {
    let meta = e.metadata();
    FileEntry {
        name: e.file_name(),
        is_dir: e.file_type().is_dir(),
        size: meta.size.unwrap_or(0),
        mtime: meta.mtime.map(|t| t as i64).unwrap_or(0),
        permissions: meta.permissions.unwrap_or(0),
        uid: meta.uid.unwrap_or(0),
        gid: meta.gid.unwrap_or(0),
        owner: meta.user.unwrap_or_default(),
        group: meta.group.unwrap_or_default(),
        is_symlink: e.file_type().is_symlink(),
        is_broken_symlink: false,
    }
}

/// 远程 → 本地
type DownloadChunkResult = Result<(u64, Vec<u8>, tokio::sync::OwnedSemaphorePermit), String>;

async fn download(
    sftp: Arc<SftpSession>,
    id: u64,
    remote: &str,
    local: &str,
    control: Arc<TransferControl>,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let source_metadata = sftp
        .metadata(remote)
        .await
        .map_err(|e| format!("无法读取远程文件信息 {remote}: {e}"))?;
    let total = source_metadata
        .size
        .ok_or_else(|| format!("远程文件大小未知: {remote}"))?;
    let source_tag = remote_source_tag(total, source_metadata.mtime);
    let partial = local_partial_path(local);
    let resume = prepare_local_partial(&partial, &source_tag, total).await?;
    let mut dst = open_local_download_partial(&partial, resume).await?;
    let mut done = resume;
    let mut last_progress = Instant::now() - Duration::from_millis(100);
    send_progress(
        event_tx,
        ctx,
        &mut last_progress,
        (id, "download", remote, done, total),
    );
    // 八个 worker 各自长期保持一个 SFTP 文件句柄，避免大文件每 4 MiB
    // 重新 open/close。信号量 permit 随结果一起保留到落盘，确保乱序缓冲
    // 不会因某个慢分块无界增长（最多约 8 × 4 MiB）。
    const READ_CHUNK: u64 = 4 * 1024 * 1024;
    const READ_CONCURRENCY: usize = 8;
    let worker_count = ((total - resume).saturating_add(READ_CHUNK - 1) / READ_CHUNK)
        .min(READ_CONCURRENCY as u64) as usize;
    let buffered_chunks = Arc::new(tokio::sync::Semaphore::new(worker_count.max(1)));
    let (chunk_tx, mut chunk_rx) =
        tokio::sync::mpsc::channel::<DownloadChunkResult>(worker_count.max(1));
    let mut tasks = tokio::task::JoinSet::new();
    for worker in 0..worker_count {
        tasks.spawn(download_file_worker(
            Arc::clone(&sftp),
            remote.to_string(),
            resume,
            total,
            READ_CHUNK,
            worker,
            worker_count,
            Arc::clone(&control),
            Arc::clone(&buffered_chunks),
            chunk_tx.clone(),
        ));
    }
    drop(chunk_tx);
    let mut ready =
        std::collections::BTreeMap::<u64, (Vec<u8>, tokio::sync::OwnedSemaphorePermit)>::new();
    let mut write_offset = resume;
    while write_offset < total {
        control.wait_async().await?;
        let result = tokio::select! {
            result = chunk_rx.recv() => result
                .ok_or_else(|| "并发下载 worker 在文件完成前全部退出".to_string())?,
            () = control.wait_canceled() => return Err("传输已取消".into()),
        };
        let (offset, data, permit) = result?;
        if ready.insert(offset, (data, permit)).is_some() {
            return Err(format!("并发下载返回重复偏移: {offset}"));
        }
        while let Some((data, _permit)) = ready.remove(&write_offset) {
            dst.write_all(&data)
                .await
                .map_err(|e| format!("写入本地文件 {local} 失败: {e}"))?;
            write_offset += data.len() as u64;
            done += data.len() as u64;
            send_progress(
                event_tx,
                ctx,
                &mut last_progress,
                (id, "download", remote, done, total),
            );
        }
    }
    drop(chunk_rx);
    while let Some(result) = tasks.join_next().await {
        result.map_err(|error| format!("并发下载 worker 崩溃: {error}"))??;
    }
    if done != total {
        return Err(format!(
            "下载不完整：预期 {total} 字节，实际收到 {done} 字节；临时文件已保留"
        ));
    }
    let final_metadata = sftp
        .metadata(remote)
        .await
        .map_err(|e| format!("重新读取远程文件信息 {remote} 失败: {e}"))?;
    if remote_source_tag(final_metadata.size.unwrap_or(0), final_metadata.mtime) != source_tag {
        return Err("下载期间远程源文件发生变化；临时文件已保留但下次会安全重置".into());
    }
    control.wait_async().await?;
    dst.flush()
        .await
        .map_err(|e| format!("刷新本地临时文件 {partial} 失败: {e}"))?;
    dst.sync_all()
        .await
        .map_err(|e| format!("同步本地临时文件 {partial} 失败: {e}"))?;
    drop(dst);
    commit_local_partial(&partial, local).await
}

#[allow(clippy::too_many_arguments)]
async fn download_file_worker(
    sftp: Arc<SftpSession>,
    remote: String,
    resume: u64,
    total: u64,
    chunk_size: u64,
    worker: usize,
    worker_count: usize,
    control: Arc<TransferControl>,
    buffered_chunks: Arc<tokio::sync::Semaphore>,
    chunk_tx: tokio::sync::mpsc::Sender<DownloadChunkResult>,
) -> Result<(), String> {
    let result: Result<(), String> = async {
        let mut file = sftp
            .open(&remote)
            .await
            .map_err(|error| format!("打开远程文件 {remote} 失败: {error}"))?;
        let stride = chunk_size.saturating_mul(worker_count as u64);
        let mut offset = resume.saturating_add(chunk_size.saturating_mul(worker as u64));
        while offset < total {
            control.wait_async().await?;
            let permit = Arc::clone(&buffered_chunks)
                .acquire_owned()
                .await
                .map_err(|_| "下载缓冲信号量已关闭".to_string())?;
            let len = (total - offset).min(chunk_size) as usize;
            let data = read_remote_chunk_from_file(&mut file, &remote, offset, len, Some(&control))
                .await?;
            chunk_tx
                .send(Ok((offset, data, permit)))
                .await
                .map_err(|_| "下载结果接收端已关闭".to_string())?;
            offset = offset.saturating_add(stride);
        }
        Ok(())
    }
    .await;
    if let Err(error) = &result {
        let _ = chunk_tx.send(Err(error.clone())).await;
    }
    result
}

async fn read_remote_chunk(
    sftp: Arc<SftpSession>,
    remote: String,
    offset: u64,
    len: usize,
    control: Option<Arc<TransferControl>>,
) -> Result<(u64, Vec<u8>), String> {
    let mut file = sftp
        .open(&remote)
        .await
        .map_err(|e| format!("打开远程文件 {remote} 失败: {e}"))?;
    let data =
        read_remote_chunk_from_file(&mut file, &remote, offset, len, control.as_deref()).await?;
    Ok((offset, data))
}

async fn read_remote_chunk_from_file(
    file: &mut russh_sftp::client::fs::File,
    remote: &str,
    offset: u64,
    len: usize,
    control: Option<&TransferControl>,
) -> Result<Vec<u8>, String> {
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("定位远程文件 {remote} 失败: {e}"))?;
    let mut data = vec![0u8; len];
    let mut read = 0usize;
    while read < len {
        if let Some(control) = control {
            control.wait_async().await?;
        }
        let count = if let Some(control) = control {
            tokio::select! {
                result = file.read(&mut data[read..]) => {
                    result.map_err(|error| format!("读取远程文件 {remote} 失败: {error}"))?
                }
                () = control.wait_canceled() => return Err("传输已取消".into()),
            }
        } else {
            file.read(&mut data[read..])
                .await
                .map_err(|error| format!("读取远程文件 {remote} 失败: {error}"))?
        };
        if count == 0 {
            return Err(format!(
                "远程文件提前结束：偏移 {offset}，预期读取 {len} 字节，实际 {read} 字节"
            ));
        }
        read += count;
    }
    Ok(data)
}

/// 本地 → 远程
async fn upload(
    sftp: &SftpSession,
    id: u64,
    local: &str,
    remote: &str,
    control: &TransferControl,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let local_metadata = tokio::fs::metadata(local)
        .await
        .map_err(|e| format!("无法读取本地文件 {local}: {e}"))?;
    let total = local_metadata.len();
    let source_tag = local_source_tag(&local_metadata);
    let mut src = tokio::fs::File::open(local)
        .await
        .map_err(|e| format!("无法打开本地文件 {local}: {e}"))?;
    let partial = remote_partial_path(remote);
    let resume = prepare_remote_partial(sftp, &partial, &source_tag, total).await?;
    src.seek(std::io::SeekFrom::Start(resume))
        .await
        .map_err(|e| format!("定位本地文件 {local} 失败: {e}"))?;
    let mut dst = sftp
        .open_with_flags(&partial, OpenFlags::CREATE | OpenFlags::WRITE)
        .await
        .map_err(|e| format!("无法创建远程临时文件 {partial}: {e}"))?;
    dst.seek(std::io::SeekFrom::Start(resume))
        .await
        .map_err(|e| format!("定位远程临时文件 {partial} 失败: {e}"))?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = resume;
    let mut last_progress = Instant::now() - Duration::from_millis(100);
    send_progress(
        event_tx,
        ctx,
        &mut last_progress,
        (id, "upload", local, done, total),
    );
    loop {
        control.wait_async().await?;
        let n = src
            .read(&mut buf)
            .await
            .map_err(|e| format!("读取本地文件 {local} 失败: {e}"))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .await
            .map_err(|e| format!("写入远程文件 {remote} 失败: {e}"))?;
        done += n as u64;
        send_progress(
            event_tx,
            ctx,
            &mut last_progress,
            (id, "upload", local, done, total),
        );
    }
    dst.shutdown()
        .await
        .map_err(|e| format!("关闭远程临时文件 {partial} 失败: {e}"))?;
    if done != total {
        return Err(format!(
            "上传不完整：预期 {total} 字节，实际写入 {done} 字节；临时文件已保留"
        ));
    }
    let final_local_metadata = tokio::fs::metadata(local)
        .await
        .map_err(|e| format!("重新读取本地文件 {local} 信息失败: {e}"))?;
    if local_source_tag(&final_local_metadata) != source_tag {
        return Err("上传期间本地源文件发生变化；远程临时文件已保留但下次会安全重置".into());
    }
    control.wait_async().await?;
    let remote_size = sftp
        .metadata(&partial)
        .await
        .map_err(|e| format!("验证远程临时文件 {partial} 失败: {e}"))?
        .size
        .unwrap_or(0);
    if remote_size != total {
        return Err(format!(
            "远程文件大小校验失败：预期 {total} 字节，实际 {remote_size} 字节"
        ));
    }
    commit_remote_partial(sftp, &partial, remote).await
}

fn local_partial_path(target: &str) -> String {
    format!("{target}.rshell-part")
}

fn remote_partial_path(target: &str) -> String {
    format!("{target}.rshell-part")
}

fn partial_meta_path(partial: &str) -> String {
    format!("{partial}.meta")
}

fn remote_source_tag(size: u64, mtime: Option<u32>) -> String {
    format!("{size}:{}", mtime.unwrap_or(0))
}

fn local_source_tag(metadata: &std::fs::Metadata) -> String {
    use std::time::UNIX_EPOCH;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    format!("{}:{modified}", metadata.len())
}

async fn prepare_local_partial(partial: &str, source_tag: &str, total: u64) -> Result<u64, String> {
    let meta_path = partial_meta_path(partial);
    reject_local_link_if_exists(std::path::Path::new(partial)).await?;
    reject_local_link_if_exists(std::path::Path::new(&meta_path)).await?;
    let stored_tag = tokio::fs::read_to_string(&meta_path)
        .await
        .unwrap_or_default();
    let size = tokio::fs::symlink_metadata(partial)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if stored_tag.trim() == source_tag && size <= total {
        return Ok(size);
    }
    let _ = tokio::fs::remove_file(partial).await;
    let mut meta = open_local_download_partial(&meta_path, 0).await?;
    meta.write_all(source_tag.as_bytes())
        .await
        .map_err(|error| format!("写入断点信息 {meta_path} 失败: {error}"))?;
    meta.flush()
        .await
        .map_err(|error| format!("刷新断点信息 {meta_path} 失败: {error}"))?;
    Ok(0)
}

async fn open_local_download_partial(path: &str, offset: u64) -> Result<tokio::fs::File, String> {
    let path_ref = std::path::Path::new(path);
    reject_local_link_if_exists(path_ref).await?;
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).write(true).truncate(false);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    let mut file = options
        .open(path_ref)
        .await
        .map_err(|error| format!("创建下载临时文件 {} 失败: {error}", path_ref.display()))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|error| format!("定位下载临时文件 {} 失败: {error}", path_ref.display()))?;
    if offset == 0 {
        file.set_len(0)
            .await
            .map_err(|error| format!("重置下载临时文件 {} 失败: {error}", path_ref.display()))?;
    }
    Ok(file)
}

async fn prepare_remote_partial(
    sftp: &SftpSession,
    partial: &str,
    source_tag: &str,
    total: u64,
) -> Result<u64, String> {
    let meta_path = partial_meta_path(partial);
    let stored_tag = sftp
        .read(&meta_path)
        .await
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    let size = sftp
        .metadata(partial)
        .await
        .ok()
        .and_then(|metadata| metadata.size)
        .unwrap_or(0);
    if stored_tag.trim() == source_tag && size <= total {
        return Ok(size);
    }
    let _ = sftp.remove_file(partial).await;
    // russh-sftp 的 `Session::write` 只带 WRITE 标志，不会创建新文件。
    // 首次上传时断点元数据尚不存在，必须显式 CREATE + TRUNCATE。
    let mut meta = sftp
        .open_with_flags(
            &meta_path,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .map_err(|e| format!("创建远程断点信息 {meta_path} 失败: {e}"))?;
    meta.write_all(source_tag.as_bytes())
        .await
        .map_err(|e| format!("写入远程断点信息 {meta_path} 失败: {e}"))?;
    meta.shutdown()
        .await
        .map_err(|e| format!("关闭远程断点信息 {meta_path} 失败: {e}"))?;
    Ok(0)
}

async fn commit_local_partial(partial: &str, target: &str) -> Result<(), String> {
    tokio::fs::rename(partial, target)
        .await
        .map_err(|e| format!("提交下载文件 {target} 失败: {e}"))?;
    let _ = tokio::fs::remove_file(partial_meta_path(partial)).await;
    Ok(())
}

async fn commit_remote_partial(
    sftp: &SftpSession,
    partial: &str,
    target: &str,
) -> Result<(), String> {
    if sftp.metadata(target).await.is_err() {
        sftp.rename(partial, target)
            .await
            .map_err(|e| format!("提交远程文件 {target} 失败: {e}"))?;
        let _ = sftp.remove_file(partial_meta_path(partial)).await;
        return Ok(());
    }
    let backup = format!("{target}.rshell-backup");
    let _ = sftp.remove_file(&backup).await;
    sftp.rename(target, &backup)
        .await
        .map_err(|e| format!("保护原远程文件 {target} 失败: {e}"))?;
    match sftp.rename(partial, target).await {
        Ok(()) => {
            let _ = sftp.remove_file(&backup).await;
            let _ = sftp.remove_file(partial_meta_path(partial)).await;
            Ok(())
        }
        Err(error) => {
            let rollback = sftp.rename(&backup, target).await;
            Err(match rollback {
                Ok(()) => format!("提交远程文件 {target} 失败，原文件已恢复: {error}"),
                Err(rollback_error) => {
                    format!("提交远程文件 {target} 失败且自动恢复失败: {error}; {rollback_error}")
                }
            })
        }
    }
}

fn send_progress(
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
    last: &mut Instant,
    (id, op, path, done, total): (u64, &'static str, &str, u64, u64),
) {
    if last.elapsed() < Duration::from_millis(100) {
        return;
    }
    let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::Progress {
        id,
        op,
        path: path.to_string(),
        done,
        total,
    }));
    *last = Instant::now();
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn pre_shell_commands_keep_latest_size_and_input() {
        let (mut cols, mut rows) = (80, 24);
        let mut input = VecDeque::new();
        assert!(stage_pre_shell_command(
            Some(SessionCmd::Resize {
                cols: 132,
                rows: 38,
            }),
            &mut cols,
            &mut rows,
            &mut input,
        ));
        assert!(stage_pre_shell_command(
            Some(SessionCmd::Resize {
                cols: 160,
                rows: 42,
            }),
            &mut cols,
            &mut rows,
            &mut input,
        ));
        assert!(stage_pre_shell_command(
            Some(SessionCmd::Input(b"echo long command".to_vec())),
            &mut cols,
            &mut rows,
            &mut input,
        ));

        assert_eq!((cols, rows), (160, 42));
        assert_eq!(
            input.pop_front().as_deref(),
            Some(b"echo long command".as_slice())
        );
        assert!(!stage_pre_shell_command(
            Some(SessionCmd::Shutdown),
            &mut cols,
            &mut rows,
            &mut input,
        ));
    }

    #[test]
    fn transport_windows_are_tuned_for_high_latency_links() {
        let config = ssh_client_config();
        assert_eq!(config.window_size, 8 * 1024 * 1024);
        assert_eq!(config.maximum_packet_size, u16::MAX as u32);
        assert!(config.nodelay);
        assert_eq!(config.keepalive_max, 3);
    }

    #[test]
    fn keyboard_interactive_is_used_only_when_the_server_offers_it() {
        let with_interactive = client::AuthResult::Failure {
            remaining_methods: russh::MethodSet::from(
                &[russh::MethodKind::KeyboardInteractive][..],
            ),
            partial_success: true,
        };
        let password_only = client::AuthResult::Failure {
            remaining_methods: russh::MethodSet::from(&[russh::MethodKind::Password][..]),
            partial_success: false,
        };
        assert!(auth_result_allows_keyboard_interactive(&with_interactive));
        assert!(!auth_result_allows_keyboard_interactive(&password_only));
        assert!(!auth_result_allows_keyboard_interactive(
            &client::AuthResult::Success
        ));
    }

    #[test]
    fn password_prompt_detection_supports_common_pam_wording() {
        for prompt in ["Password:", "Enter passphrase", "登录密码："] {
            assert!(looks_like_password_prompt(prompt), "missed {prompt:?}");
        }
        assert!(!looks_like_password_prompt("Verification code:"));
        assert!(!looks_like_password_prompt("OTP:"));
    }

    #[cfg(unix)]
    #[test]
    fn download_root_rejects_symlink_components() {
        let root = std::env::temp_dir().join(format!(
            "rshell-download-root-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let outside = root.join("outside");
        let link = root.join("link");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(ensure_safe_download_root(&link.join("nested")))
            .unwrap_err();
        assert!(error.contains("符号链接"), "{error}");
        assert!(!outside.join("nested").exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn digest_hex_parser_rejects_malformed_remote_output() {
        let digest = std::array::from_fn(|index| index as u8);
        let encoded = hex_digest(digest);
        assert_eq!(parse_hex_digest(&encoded), Some(digest));
        assert_eq!(parse_hex_digest(&encoded.to_uppercase()), Some(digest));
        assert_eq!(parse_hex_digest("abcd"), None);
        assert_eq!(parse_hex_digest(&"z".repeat(64)), None);
    }

    #[test]
    fn remote_metadata_updates_never_include_a_truncating_size() {
        let mode_only = remote_mode_attributes(0o600);
        assert_eq!(mode_only.permissions, Some(0o600));
        assert_eq!(mode_only.size, None);
        assert_eq!(mode_only.uid, None);
        assert_eq!(mode_only.gid, None);
        assert_eq!(mode_only.atime, None);
        assert_eq!(mode_only.mtime, None);

        let current = russh_sftp::protocol::FileAttributes {
            size: Some(17),
            uid: Some(1000),
            gid: Some(1000),
            permissions: Some(0o100600),
            ..russh_sftp::protocol::FileAttributes::empty()
        };
        let original = russh_sftp::protocol::FileAttributes {
            size: Some(23),
            uid: Some(1001),
            gid: Some(1002),
            permissions: Some(0o100640),
            atime: Some(1),
            mtime: Some(2),
            ..russh_sftp::protocol::FileAttributes::empty()
        };
        let preserved = preserved_remote_attributes(&current, &original);
        assert_eq!(preserved.uid, original.uid);
        assert_eq!(preserved.gid, original.gid);
        assert_eq!(preserved.permissions, original.permissions);
        assert_eq!(preserved.size, None);
        assert_eq!(preserved.atime, None);
        assert_eq!(preserved.mtime, None);
    }

    #[test]
    fn only_mutations_use_the_serial_operation_lane() {
        assert!(is_mutating_sftp_command(&SftpCmd::Mkdir("/tmp/a".into())));
        assert!(is_mutating_sftp_command(&SftpCmd::Rename {
            from: "/tmp/a".into(),
            to: "/tmp/b".into(),
        }));
        assert!(!is_mutating_sftp_command(&SftpCmd::List("/tmp".into())));
        assert!(!is_mutating_sftp_command(&SftpCmd::ReadFile {
            id: 1,
            path: "/tmp/a".into(),
            max_bytes: 1024,
        }));
    }

    #[test]
    fn bulk_connection_accepts_only_the_primary_connection_fingerprint() {
        let trusted =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let other =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let fingerprint = trusted
            .public_key()
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut handler = TrustedClientHandler { fingerprint };
            assert!(
                handler
                    .check_server_key(trusted.public_key())
                    .await
                    .unwrap()
            );
            assert!(!handler.check_server_key(other.public_key()).await.unwrap());
        });
    }

    #[test]
    fn background_monitor_waits_until_reactivated() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (active_tx, mut active_rx) = tokio::sync::watch::channel(false);
            let mut wait =
                tokio::spawn(async move { wait_for_monitor_reactivation(&mut active_rx).await });
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut wait)
                    .await
                    .is_err()
            );
            active_tx.send(true).unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(200), wait)
                    .await
                    .unwrap()
                    .unwrap()
            );
        });
    }

    #[test]
    fn monitor_frames_survive_packet_splits_and_queue_multiple_samples() {
        let first = format!("noise{MONITOR_FRAME_BEGIN}\n=STAT\none\n{MONITOR_FRAME_END}");
        let second = format!("{MONITOR_FRAME_BEGIN}\n=LOAD\ntwo\n{MONITOR_FRAME_END}");
        let mut buffer = first.as_bytes()[..19].to_vec();
        assert!(take_monitor_frame(&mut buffer).is_none());
        buffer.extend_from_slice(&first.as_bytes()[19..]);
        buffer.extend_from_slice(second.as_bytes());
        assert_eq!(
            take_monitor_frame(&mut buffer).as_deref(),
            Some("=STAT\none")
        );
        assert_eq!(
            take_monitor_frame(&mut buffer).as_deref(),
            Some("=LOAD\ntwo")
        );
        assert!(take_monitor_frame(&mut buffer).is_none());
    }

    #[test]
    fn persistent_monitor_runs_expensive_sections_every_fifth_sample() {
        let command = monitor_stream_command();
        assert!(command.contains(MONITOR_DYNAMIC_CMD));
        assert!(command.contains(MONITOR_DISKS_CMD));
        assert!(command.contains(MONITOR_PROCESSES_CMD));
        assert!(command.contains("sample % 5"));
        assert!(command.contains(MONITOR_FRAME_BEGIN));
        assert!(command.contains(MONITOR_FRAME_END));
    }

    #[cfg(unix)]
    #[test]
    fn persistent_monitor_command_is_valid_posix_shell_syntax() {
        let output = std::process::Command::new("/bin/sh")
            .args(["-n", "-c", &monitor_stream_command()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_quote_roundtrips_metacharacters() {
        let value = "a 'quoted' path; $(touch should-not-run)\nnext";
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote(value)))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, value.as_bytes());
    }

    #[test]
    fn privileged_save_failures_keep_actionable_categories() {
        let conflict = classify_privileged_failure(ExecOutput {
            status: Some(72),
            stdout: String::new(),
            stderr: String::new(),
        });
        assert_eq!(conflict.kind, WriteFailureKind::Conflict);

        let authentication = classify_privileged_failure(ExecOutput {
            status: Some(1),
            stdout: String::new(),
            stderr: "sudo: a password is required".into(),
        });
        assert_eq!(authentication.kind, WriteFailureKind::Authentication);

        let validation = classify_privileged_failure(ExecOutput {
            status: Some(78),
            stdout: String::new(),
            stderr: "syntax error".into(),
        });
        assert_eq!(validation.kind, WriteFailureKind::Validation);

        let policy = classify_privileged_failure(ExecOutput {
            status: Some(1),
            stdout: String::new(),
            stderr: "user is not in the sudoers file".into(),
        });
        assert_eq!(policy.kind, WriteFailureKind::Unsupported);
    }

    #[cfg(unix)]
    #[test]
    fn privileged_script_atomically_saves_and_detects_conflict() {
        use sha2::{Digest as _, Sha256};
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!(
            "rshell-privileged-save-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("quo'ted config");
        let stage = dir.join("stage file");
        let root_temp = dir.join(".root-part");
        std::fs::write(&target, b"before\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::write(&stage, b"after\n").unwrap();
        let expected = hex_digest(Sha256::digest(b"before\n").into());
        let wanted = hex_digest(Sha256::digest(b"after\n").into());

        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(PRIVILEGED_SAVE_SCRIPT)
            .arg("rshell-save")
            .arg(&target)
            .arg(&stage)
            .arg(&expected)
            .arg(&wanted)
            .arg("0640")
            .arg("-")
            .arg(&root_temp)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"after\n");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        assert!(!stage.exists());
        assert!(!root_temp.exists());

        let conflict_stage = dir.join("conflict stage");
        let conflict_temp = dir.join(".conflict-root-part");
        std::fs::write(&conflict_stage, b"new edit\n").unwrap();
        let conflict_wanted = hex_digest(Sha256::digest(b"new edit\n").into());
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(PRIVILEGED_SAVE_SCRIPT)
            .arg("rshell-save")
            .arg(&target)
            .arg(&conflict_stage)
            .arg(&expected)
            .arg(&conflict_wanted)
            .arg("0640")
            .arg("-")
            .arg(&conflict_temp)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(72));
        assert_eq!(std::fs::read(&target).unwrap(), b"after\n");
        assert!(!conflict_temp.exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// 内置测试 SSH 服务器:Keyboard-Interactive 密码 test123,shell 通道回显输入
    struct TestServer;

    impl russh::server::Handler for TestServer {
        type Error = russh::Error;

        async fn auth_password(
            &mut self,
            _user: &str,
            _password: &str,
        ) -> Result<russh::server::Auth, Self::Error> {
            Ok(russh::server::Auth::Reject {
                proceed_with_methods: Some(russh::MethodSet::from(
                    &[russh::MethodKind::KeyboardInteractive][..],
                )),
                partial_success: false,
            })
        }

        async fn auth_keyboard_interactive<'a>(
            &'a mut self,
            user: &str,
            _submethods: &str,
            response: Option<russh::server::Response<'a>>,
        ) -> Result<russh::server::Auth, Self::Error> {
            if user != "test" {
                return Ok(russh::server::Auth::reject());
            }
            match response {
                None => Ok(russh::server::Auth::Partial {
                    name: "RShell test".into(),
                    instructions: "Enter the loopback credential".into(),
                    prompts: vec![("Password: ".into(), false)].into(),
                }),
                Some(mut responses) => {
                    if responses.next().as_deref() == Some(b"test123") {
                        Ok(russh::server::Auth::Accept)
                    } else {
                        Ok(russh::server::Auth::reject())
                    }
                }
            }
        }

        async fn channel_open_session(
            &mut self,
            _channel: russh::Channel<russh::server::Msg>,
            reply: russh::server::ChannelOpenHandle,
            _session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }

        async fn shell_request(
            &mut self,
            channel: russh::ChannelId,
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            session.data(channel, "RSHELL_SRV_GREETING\r\n")?;
            Ok(())
        }

        async fn data(
            &mut self,
            channel: russh::ChannelId,
            data: &[u8],
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            session.data(
                channel,
                format!("echo:{}\r\n", String::from_utf8_lossy(data)),
            )?;
            Ok(())
        }
    }

    /// 回环测试:本进程起 SSH 服务器,验证 连接→认证→pty→shell→双向数据
    #[test]
    fn ssh_loopback() {
        let _ = env_logger::builder()
            // russh 的 DEBUG 帧会打印认证报文字节；测试也不能把密码写进日志。
            .filter_level(log::LevelFilter::Warn)
            .is_test(true)
            .try_init();
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let server_config = Arc::new(russh::server::Config {
            keys: vec![key],
            ..Default::default()
        });

        // 服务器线程
        let (port_tx, port_rx) = channel::<u16>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                port_tx.send(listener.local_addr().unwrap().port()).unwrap();
                let (socket, _) = listener.accept().await.unwrap();
                // run_stream 在后台跑连接,返回的 RunningSession 需要 await 以驱动会话
                match russh::server::run_stream(server_config, socket, TestServer).await {
                    Ok(session) => {
                        if let Err(e) = session.await {
                            eprintln!("[test-server] session error: {e:?}");
                        }
                    }
                    Err(e) => eprintln!("[test-server] connect error: {e:?}"),
                }
            });
        });
        let port = port_rx.recv().unwrap();

        // 客户端(被测代码)
        let mut ch = spawn(
            SshParams {
                host: "127.0.0.1".into(),
                port,
                user: "test".into(),
                auth: SshAuth::Password("test123".into()),
            },
            80,
            24,
            egui::Context::default(),
        );

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut output = String::new();
        let mut greeted = false;
        while Instant::now() < deadline {
            while let Ok(data) = ch.data_rx.try_recv() {
                output.push_str(&String::from_utf8_lossy(&data));
                if !greeted && output.contains("RSHELL_SRV_GREETING") {
                    greeted = true;
                    ch.cmd_tx
                        .send(SessionCmd::Input(b"hello_rshell".to_vec()))
                        .unwrap();
                }
                if greeted && output.contains("echo:hello_rshell") {
                    return;
                }
            }
            match ch.event_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(SessionEvent::VerifyHostKey(p)) => {
                    // TOFU:测试中直接仅本次接受
                    p.reply.send(HostKeyVerdict::AcceptOnce).unwrap();
                }
                Ok(SessionEvent::Exited) => panic!("session exited early. output:\n{output}"),
                Ok(SessionEvent::Metrics(_)) => {}
                Ok(SessionEvent::Sftp(_)) => {}
                Ok(SessionEvent::KeyboardInteractive(_)) => {}
                Err(_) => {}
            }
        }
        panic!("timeout. output:\n{output}");
    }
}
