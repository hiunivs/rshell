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
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::files::{FileEntry, SftpCmd, SftpEvent, SftpRx, TransferControl, sftp_channel};
use super::monitor::{
    MONITOR_DISKS_CMD, MONITOR_DYNAMIC_CMD, MONITOR_STATIC_CMD, MonitorState, SystemMetrics,
};
use super::{
    CmdRx, DataTx, HostKeyPrompt, HostKeyVerdict, SessionChannels, SessionCmd, SessionEvent,
    cmd_channel, data_channel,
};

/// 认证方式
#[derive(Clone)]
pub enum SshAuth {
    Password(String),
    KeyFile {
        path: String,
        passphrase: Option<String>,
    },
}

#[derive(Clone)]
pub struct SshParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
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
            HostKeyStatus::Match => Ok(true),
            status => {
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
                match verdict {
                    Ok(Some(HostKeyVerdict::AcceptAndSave)) => {
                        store.record(
                            &self.host,
                            self.port,
                            KnownHostKey {
                                algorithm,
                                fingerprint,
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
                            Ok(true)
                        }
                    }
                    Ok(Some(HostKeyVerdict::AcceptOnce)) => Ok(true),
                    _ => Ok(false),
                }
            }
        }
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

    let config = Arc::new(Config {
        nodelay: true,
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let (verdict_tx, verdict_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler = ClientHandler {
        host: params.host.clone(),
        port: params.port,
        event_tx: event_tx.clone(),
        data_tx: data_tx.clone(),
        verdict_tx,
        verdict_rx,
        ctx: ctx.clone(),
    };
    let connect = client::connect(config, (params.host.as_str(), params.port), handler);
    tokio::pin!(connect);
    let mut handle = loop {
        tokio::select! {
            result = &mut connect => match result {
                Ok(handle) => break handle,
                Err(error) => fatal!("连接失败:{error}"),
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
        SshAuth::Password(pass) => {
            handle
                .authenticate_password(params.user.clone(), pass.clone())
                .await
        }
        SshAuth::KeyFile { path, passphrase } => {
            match load_secret_key(path, passphrase.as_deref()) {
                Ok(key) => {
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
                }
                Err(e) => fatal!("加载私钥失败({path}):{e}"),
            }
        }
    };
    match auth_result {
        Ok(r) if r.success() => status!("认证成功"),
        Ok(r) => fatal!("认证失败:{r:?}"),
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
    let mut channel = match handle.channel_open_session().await {
        Ok(c) => c,
        Err(e) => fatal!("打开通道失败:{e}"),
    };
    if let Err(e) = channel
        .request_pty(
            false,
            "xterm-256color",
            pty_cols as u32,
            pty_rows as u32,
            0,
            0,
            &[],
        )
        .await
    {
        fatal!("请求 PTY 失败:{e}");
    }
    if let Err(e) = channel.request_shell(false).await {
        fatal!("启动 shell 失败:{e}");
    }
    while let Some(data) = pending_input.pop_front() {
        if channel.data_bytes(data).await.is_err() {
            let _ = event_tx.send(SessionEvent::Exited);
            ctx.request_repaint();
            return;
        }
    }

    // ---- 监控任务:同一连接上另开 exec 通道,1s 一轮 ----
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
        tokio::spawn(async move {
            sftp_loop(sftp_handle, sftp_rx, sftp_tx_ev, sftp_ctx).await;
        });
    }

    // ---- 主循环:通道数据 <-> UI ----
    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        if send_terminal_data(&data_tx, &data).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if send_terminal_data(&data_tx, &data).await.is_err() {
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

async fn send_terminal_data(data_tx: &DataTx, data: &[u8]) -> Result<(), ()> {
    for chunk in data.chunks(64 * 1024) {
        data_tx.send(chunk.to_vec()).await.map_err(|_| ())?;
    }
    Ok(())
}

/// 每秒通过一条 exec 命令采集系统指标;shutdown 置位或连接断开即退出
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
    let mut next_disk_sample = Instant::now();
    // 等 shell 先起来
    tokio::time::sleep(Duration::from_millis(500)).await;
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if !*active_rx.borrow() {
            if !wait_for_next_monitor_sample(&mut active_rx).await {
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

        // 当前标签每 5 秒更新磁盘；后台标签暂停监控，不再执行 df。
        if Instant::now() >= next_disk_sample {
            if let Ok(text) = run_exec(&handle, MONITOR_DISKS_CMD).await {
                disks = MonitorState::default().update(&text, 0).disks;
            }
            next_disk_sample = Instant::now() + Duration::from_secs(5);
        }

        let t0 = Instant::now();
        match run_exec(&handle, MONITOR_DYNAMIC_CMD).await {
            Ok(text) => {
                if consecutive_errors > 0 {
                    let _ = data_tx
                        .send(ansi("\x1b[36m系统监控已恢复\x1b[0m\r\n"))
                        .await;
                }
                consecutive_errors = 0;
                let rtt = t0.elapsed().as_millis() as u32;
                let mut metrics = state.update(&text, rtt);
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
                metrics.disks.clone_from(&disks);
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
            Err(error) => {
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
        if !wait_for_next_monitor_sample(&mut active_rx).await {
            break;
        }
    }
}

/// 前台标签每秒采样；后台标签完全暂停。重新变为前台时立即采样。
async fn wait_for_next_monitor_sample(active_rx: &mut tokio::sync::watch::Receiver<bool>) -> bool {
    loop {
        let active = *active_rx.borrow();
        if !active {
            if active_rx.changed().await.is_err() {
                return false;
            }
            if *active_rx.borrow() {
                return true;
            }
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => return true,
            changed = active_rx.changed() => {
                if changed.is_err() {
                    return false;
                }
                if *active_rx.borrow() {
                    return true;
                }
                // 刚切到后台：等待再次激活，不执行后台采样。
            }
        }
    }
}

/// 执行一条命令并收集全部 stdout
async fn run_exec(
    handle: &client::Handle<ClientHandler>,
    cmd: &str,
) -> Result<String, russh::Error> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(false, cmd).await?;
    let mut out = Vec::new();
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

// ---------------------------------------------------------------------------
// SFTP
// ---------------------------------------------------------------------------

async fn sftp_loop(
    handle: Arc<client::Handle<ClientHandler>>,
    mut rx: SftpRx,
    event_tx: Sender<SessionEvent>,
    ctx: egui::Context,
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
    while let Some(cmd) = rx.recv().await {
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
                        run_remote_transfer(task_sftp, cmd, &control, &task_event_tx, &task_ctx)
                            .await
                    } else {
                        failed_sftp_command(cmd, "传输已取消")
                    };
                    let _ = task_event_tx.send(SessionEvent::Sftp(result));
                    task_controls.lock().expect("transfer controls").remove(&id);
                    task_ctx.request_repaint();
                });
                continue;
            }
            SftpCmd::List(path) => {
                let result = list_dir_resolved(&sftp, &path).await;
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::Listed { path, result }));
            }
            SftpCmd::Mkdir(path) => {
                let r = sftp.create_dir(path).await.map_err(|e| e.to_string());
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::OpDone {
                    id: None,
                    op: "mkdir",
                    result: r,
                }));
            }
            SftpCmd::Remove { path, is_dir } => {
                let r = if is_dir {
                    sftp.remove_dir(path).await.map_err(|e| e.to_string())
                } else {
                    sftp.remove_file(path).await.map_err(|e| e.to_string())
                };
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::OpDone {
                    id: None,
                    op: "remove",
                    result: r,
                }));
            }
            SftpCmd::RemoveRecursive { path } => {
                let r = remove_recursive(&sftp, &path).await;
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::OpDone {
                    id: None,
                    op: "remove",
                    result: r,
                }));
            }
            SftpCmd::Rename { from, to } => {
                let r = sftp.rename(from, to).await.map_err(|e| e.to_string());
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::OpDone {
                    id: None,
                    op: "rename",
                    result: r,
                }));
            }
            SftpCmd::SetPermissions { path, mode } => {
                let r = set_remote_perms(&sftp, &path, mode).await;
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::OpDone {
                    id: None,
                    op: "chmod",
                    result: r,
                }));
            }
            SftpCmd::ReadFile {
                id,
                path,
                max_bytes,
            } => {
                let result = read_remote_capped(&sftp, &path, max_bytes).await;
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::FileContent {
                    id,
                    path,
                    result,
                }));
            }
            SftpCmd::WriteFile { id, path, data } => {
                let result = write_remote_atomic(&sftp, &path, &data).await;
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::OpDone {
                    id: Some(id),
                    op: "write",
                    result,
                }));
            }
            SftpCmd::DetectKind { id, path } => {
                let cmd = format!("file --mime-type -b -- {}", shell_quote(&path));
                let result = run_exec(&handle, &cmd)
                    .await
                    .map(|out| out.trim().to_string())
                    .map_err(|e| e.to_string())
                    .and_then(|mime| {
                        if mime.is_empty() {
                            Err("远端没有 file 命令或输出为空".to_string())
                        } else {
                            Ok(mime)
                        }
                    });
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::KindDetected {
                    id,
                    path,
                    result,
                }));
            }
        }
        ctx.request_repaint();
    }
}

/// shell 单引号转义(路径嵌入 exec 命令行时使用)
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// 读取远程文件前 max_bytes 字节到内存(供在线预览/编辑)
async fn read_remote_capped(
    sftp: &SftpSession,
    path: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let mut file = sftp
        .open(path)
        .await
        .map_err(|e| format!("无法打开 {path}: {e}"))?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    while (out.len() as u64) < max_bytes {
        let want = (max_bytes - out.len() as u64).min(buf.len() as u64) as usize;
        let n = file
            .read(&mut buf[..want])
            .await
            .map_err(|e| format!("读取 {path} 失败: {e}"))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// 编辑器保存:先写 .rshell-part 临时文件再 rename,避免半截文件
async fn write_remote_atomic(sftp: &SftpSession, path: &str, data: &[u8]) -> Result<(), String> {
    let partial = format!("{path}.rshell-part");
    let mut file = sftp
        .open_with_flags(
            &partial,
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
        )
        .await
        .map_err(|e| format!("无法创建临时文件 {partial}: {e}"))?;
    file.write_all(data)
        .await
        .map_err(|e| format!("写入 {partial} 失败: {e}"))?;
    file.shutdown()
        .await
        .map_err(|e| format!("关闭 {partial} 失败: {e}"))?;
    sftp.rename(&partial, path)
        .await
        .map_err(|e| format!("替换 {path} 失败: {e}"))
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
    control: &TransferControl,
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
                control,
                event_tx,
                ctx,
            )
            .await,
        ),
        SftpCmd::DownloadDir { id, remote, local } => (
            id,
            "download_dir",
            download_dir(&sftp, id, &remote, &local, control, event_tx, ctx).await,
        ),
        SftpCmd::UploadDir { id, local, remote } => (
            id,
            "upload_dir",
            upload_dir(&sftp, id, &local, &remote, control, event_tx, ctx).await,
        ),
        SftpCmd::Upload { id, local, remote } => (
            id,
            "upload",
            upload(&sftp, id, &local, &remote, control, event_tx, ctx).await,
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
        SftpCmd::WriteFile { id, .. } => SftpEvent::OpDone {
            id: Some(id),
            op: "write",
            result: Err(message.to_string()),
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
        if name == "." || name == ".." {
            continue;
        }
        let child = super::files::join_path(path, &name);
        Box::pin(remove_recursive(sftp, &child)).await?;
    }
    sftp.remove_dir(path).await.map_err(|e| e.to_string())
}

/// 远程→本地目录:先遍历收集,再逐文件下载
async fn download_dir(
    sftp: &SftpSession,
    id: u64,
    remote: &str,
    local: &str,
    control: &TransferControl,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let mut done = 0u64;
    let mut last_progress = Instant::now() - Duration::from_millis(100);
    tokio::fs::create_dir_all(local)
        .await
        .map_err(|e| format!("创建目录 {local} 失败: {e}"))?;
    Box::pin(download_dir_stream(
        sftp,
        id,
        remote,
        local,
        remote,
        control,
        event_tx,
        ctx,
        &mut done,
        &mut last_progress,
    ))
    .await
}

/// 本地→远程目录:遍历本地收集,逐文件上传
async fn upload_dir(
    sftp: &SftpSession,
    id: u64,
    local: &str,
    remote: &str,
    control: &TransferControl,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let mut done = 0u64;
    let mut last_progress = Instant::now() - Duration::from_millis(100);
    ensure_remote_dir_all(sftp, remote).await?;
    Box::pin(upload_dir_stream(
        sftp,
        id,
        std::path::Path::new(local),
        remote,
        local,
        control,
        event_tx,
        ctx,
        &mut done,
        &mut last_progress,
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
async fn download_dir_stream(
    sftp: &SftpSession,
    id: u64,
    remote_dir: &str,
    local_dir: &str,
    progress_path: &str,
    control: &TransferControl,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
    done: &mut u64,
    last_progress: &mut Instant,
) -> Result<(), String> {
    control.wait_async().await?;
    let entries = sftp
        .read_dir(remote_dir)
        .await
        .map_err(|e| format!("读取远程目录 {remote_dir} 失败: {e}"))?;
    for entry in entries {
        control.wait_async().await?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let remote_path = super::files::join_path(remote_dir, &name);
        let local_path = std::path::Path::new(local_dir).join(&name);
        if entry.file_type().is_symlink() {
            return Err(format!("目录传输暂不跟随符号链接，已停止于：{remote_path}"));
        }
        if entry.file_type().is_dir() {
            tokio::fs::create_dir_all(&local_path)
                .await
                .map_err(|e| format!("创建目录 {} 失败: {e}", local_path.display()))?;
            Box::pin(download_dir_stream(
                sftp,
                id,
                &remote_path,
                &local_path.to_string_lossy(),
                progress_path,
                control,
                event_tx,
                ctx,
                done,
                last_progress,
            ))
            .await?;
        } else {
            let mut metadata = entry.metadata();
            if metadata.size.is_none() {
                metadata = sftp
                    .metadata(&remote_path)
                    .await
                    .map_err(|e| format!("读取 {remote_path} 信息失败: {e}"))?;
            }
            let expected = metadata.size.unwrap_or(0);
            let source_tag = remote_source_tag(expected, metadata.mtime);
            download_dir_file(
                sftp,
                id,
                &remote_path,
                &local_path.to_string_lossy(),
                expected,
                &source_tag,
                progress_path,
                control,
                event_tx,
                ctx,
                done,
                last_progress,
            )
            .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn download_dir_file(
    sftp: &SftpSession,
    id: u64,
    remote: &str,
    local: &str,
    expected: u64,
    source_tag: &str,
    progress_path: &str,
    control: &TransferControl,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
    done: &mut u64,
    last_progress: &mut Instant,
) -> Result<(), String> {
    let partial = local_partial_path(local);
    let resume = prepare_local_partial(&partial, source_tag, expected).await?;
    let mut src = sftp.open(remote).await.map_err(|e| e.to_string())?;
    src.seek(std::io::SeekFrom::Start(resume))
        .await
        .map_err(|e| e.to_string())?;
    let mut dst = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&partial)
        .await
        .map_err(|e| format!("创建临时文件 {partial} 失败: {e}"))?;
    dst.seek(std::io::SeekFrom::Start(resume))
        .await
        .map_err(|e| e.to_string())?;
    *done = done.saturating_add(resume);
    let mut file_done = resume;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        control.wait_async().await?;
        let n = src.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .await
            .map_err(|e| format!("写入 {partial} 失败: {e}"))?;
        file_done += n as u64;
        *done += n as u64;
        send_progress(
            event_tx,
            ctx,
            last_progress,
            (id, "download_dir", progress_path, *done, 0),
        );
    }
    if file_done != expected {
        return Err(format!(
            "下载 {remote} 不完整：预期 {expected} 字节，实际 {file_done} 字节"
        ));
    }
    let final_metadata = sftp
        .metadata(remote)
        .await
        .map_err(|e| format!("重新读取 {remote} 信息失败: {e}"))?;
    if remote_source_tag(final_metadata.size.unwrap_or(0), final_metadata.mtime) != source_tag {
        return Err(format!("下载期间远程源文件 {remote} 发生变化"));
    }
    control.wait_async().await?;
    dst.flush().await.map_err(|e| e.to_string())?;
    dst.sync_all().await.map_err(|e| e.to_string())?;
    drop(dst);
    commit_local_partial(&partial, local).await
}

#[allow(clippy::too_many_arguments)]
async fn upload_dir_stream(
    sftp: &SftpSession,
    id: u64,
    local_dir: &std::path::Path,
    remote_dir: &str,
    progress_path: &str,
    control: &TransferControl,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
    done: &mut u64,
    last_progress: &mut Instant,
) -> Result<(), String> {
    control.wait_async().await?;
    let mut entries = tokio::fs::read_dir(local_dir)
        .await
        .map_err(|e| format!("读取本地目录 {} 失败: {e}", local_dir.display()))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("遍历本地目录 {} 失败: {e}", local_dir.display()))?
    {
        control.wait_async().await?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let local_path = entry.path();
        let remote_path = super::files::join_path(remote_dir, &name);
        if entry
            .file_type()
            .await
            .map_err(|e| format!("读取 {} 类型失败: {e}", local_path.display()))?
            .is_symlink()
        {
            return Err(format!(
                "目录传输暂不跟随符号链接，已停止于：{}",
                local_path.display()
            ));
        }
        let metadata = entry
            .metadata()
            .await
            .map_err(|e| format!("读取 {} 信息失败: {e}", local_path.display()))?;
        if metadata.is_dir() {
            ensure_remote_dir_all(sftp, &remote_path).await?;
            Box::pin(upload_dir_stream(
                sftp,
                id,
                &local_path,
                &remote_path,
                progress_path,
                control,
                event_tx,
                ctx,
                done,
                last_progress,
            ))
            .await?;
        } else {
            let source_tag = local_source_tag(&metadata);
            upload_dir_file(
                sftp,
                id,
                &local_path.to_string_lossy(),
                &remote_path,
                metadata.len(),
                &source_tag,
                progress_path,
                control,
                event_tx,
                ctx,
                done,
                last_progress,
            )
            .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn upload_dir_file(
    sftp: &SftpSession,
    id: u64,
    local: &str,
    remote: &str,
    expected: u64,
    source_tag: &str,
    progress_path: &str,
    control: &TransferControl,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
    done: &mut u64,
    last_progress: &mut Instant,
) -> Result<(), String> {
    let partial = remote_partial_path(remote);
    let resume = prepare_remote_partial(sftp, &partial, source_tag, expected).await?;
    let mut src = tokio::fs::File::open(local)
        .await
        .map_err(|e| format!("打开 {local} 失败: {e}"))?;
    src.seek(std::io::SeekFrom::Start(resume))
        .await
        .map_err(|e| e.to_string())?;
    let mut dst = sftp
        .open_with_flags(&partial, OpenFlags::CREATE | OpenFlags::WRITE)
        .await
        .map_err(|e| format!("创建远程临时文件 {partial} 失败: {e}"))?;
    dst.seek(std::io::SeekFrom::Start(resume))
        .await
        .map_err(|e| e.to_string())?;
    *done = done.saturating_add(resume);
    let mut file_done = resume;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        control.wait_async().await?;
        let n = src.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .await
            .map_err(|e| format!("写入 {remote} 失败: {e}"))?;
        file_done += n as u64;
        *done += n as u64;
        send_progress(
            event_tx,
            ctx,
            last_progress,
            (id, "upload_dir", progress_path, *done, 0),
        );
    }
    dst.shutdown().await.map_err(|e| e.to_string())?;
    if file_done != expected {
        return Err(format!(
            "上传 {local} 不完整：预期 {expected} 字节，实际 {file_done} 字节"
        ));
    }
    let final_local_metadata = tokio::fs::metadata(local)
        .await
        .map_err(|e| format!("重新读取 {local} 信息失败: {e}"))?;
    if local_source_tag(&final_local_metadata) != source_tag {
        return Err(format!("上传期间本地源文件 {local} 发生变化"));
    }
    control.wait_async().await?;
    let remote_size = sftp
        .metadata(&partial)
        .await
        .map_err(|e| e.to_string())?
        .size
        .unwrap_or(0);
    if remote_size != expected {
        return Err(format!(
            "远程文件 {remote} 大小校验失败：预期 {expected}，实际 {remote_size}"
        ));
    }
    commit_remote_partial(sftp, &partial, remote).await
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
    let attrs = russh_sftp::protocol::FileAttributes {
        permissions: Some(mode),
        ..Default::default()
    };
    sftp.set_metadata(path, attrs)
        .await
        .map_err(|e| e.to_string())
}

async fn open_sftp(handle: &client::Handle<ClientHandler>) -> Result<SftpSession, String> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| e.to_string())?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| e.to_string())?;
    SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| e.to_string())
}

/// 列目录,并把符号链接解析为目标的类型(/bin、/etc 这类链接目录按目录处理)
async fn list_dir_resolved(sftp: &SftpSession, path: &str) -> Result<Vec<FileEntry>, String> {
    let rd = sftp.read_dir(path).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for e in rd {
        let mut entry = dir_entry_to_file_entry(&e);
        if entry.is_symlink {
            // stat 跟随链接;悬空链接保持原样(非目录)
            if let Ok(meta) = sftp.metadata(e.path()).await {
                entry.is_dir = meta.file_type().is_dir();
                entry.size = meta.size.unwrap_or(0);
                entry.mtime = meta.mtime.map(|t| t as i64).unwrap_or(entry.mtime);
            }
        }
        out.push(entry);
    }
    Ok(out)
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
    }
}

/// 远程 → 本地
async fn download(
    sftp: Arc<SftpSession>,
    id: u64,
    remote: &str,
    local: &str,
    control: &TransferControl,
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
    let mut dst = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&partial)
        .await
        .map_err(|e| format!("无法创建本地临时文件 {partial}: {e}"))?;
    dst.seek(std::io::SeekFrom::Start(resume))
        .await
        .map_err(|e| format!("定位本地临时文件 {partial} 失败: {e}"))?;
    let mut done = resume;
    let mut last_progress = Instant::now() - Duration::from_millis(100);
    send_progress(
        event_tx,
        ctx,
        &mut last_progress,
        (id, "download", remote, done, total),
    );
    const READ_CHUNK: u64 = 4 * 1024 * 1024;
    const READ_CONCURRENCY: usize = 4;
    let mut tasks = tokio::task::JoinSet::new();
    let mut next_offset = resume;
    for _ in 0..READ_CONCURRENCY {
        if next_offset >= total {
            break;
        }
        let len = (total - next_offset).min(READ_CHUNK) as usize;
        tasks.spawn(read_remote_chunk(
            Arc::clone(&sftp),
            remote.to_string(),
            next_offset,
            len,
        ));
        next_offset += len as u64;
    }
    let mut ready = std::collections::BTreeMap::new();
    let mut write_offset = resume;
    while let Some(result) = tasks.join_next().await {
        control.wait_async().await?;
        let (offset, data) = result.map_err(|e| format!("并行下载任务失败: {e}"))??;
        ready.insert(offset, data);
        while let Some(data) = ready.remove(&write_offset) {
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
        if next_offset < total {
            let len = (total - next_offset).min(READ_CHUNK) as usize;
            tasks.spawn(read_remote_chunk(
                Arc::clone(&sftp),
                remote.to_string(),
                next_offset,
                len,
            ));
            next_offset += len as u64;
        }
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

async fn read_remote_chunk(
    sftp: Arc<SftpSession>,
    remote: String,
    offset: u64,
    len: usize,
) -> Result<(u64, Vec<u8>), String> {
    let mut file = sftp
        .open(&remote)
        .await
        .map_err(|e| format!("打开远程文件 {remote} 失败: {e}"))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("定位远程文件 {remote} 失败: {e}"))?;
    let mut data = vec![0u8; len];
    let mut read = 0usize;
    while read < len {
        let count = file
            .read(&mut data[read..])
            .await
            .map_err(|e| format!("读取远程文件 {remote} 失败: {e}"))?;
        if count == 0 {
            return Err(format!(
                "远程文件提前结束：偏移 {offset}，预期读取 {len} 字节，实际 {read} 字节"
            ));
        }
        read += count;
    }
    Ok((offset, data))
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
    let stored_tag = tokio::fs::read_to_string(&meta_path)
        .await
        .unwrap_or_default();
    let size = tokio::fs::metadata(partial)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if stored_tag.trim() == source_tag && size <= total {
        return Ok(size);
    }
    let _ = tokio::fs::remove_file(partial).await;
    tokio::fs::write(&meta_path, source_tag)
        .await
        .map_err(|e| format!("写入断点信息 {meta_path} 失败: {e}"))?;
    Ok(0)
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
    fn background_monitor_waits_until_reactivated() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (active_tx, mut active_rx) = tokio::sync::watch::channel(false);
            let mut wait =
                tokio::spawn(async move { wait_for_next_monitor_sample(&mut active_rx).await });
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

    /// 内置测试 SSH 服务器:密码 test/test123,shell 通道回显输入
    struct TestServer;

    impl russh::server::Handler for TestServer {
        type Error = russh::Error;

        async fn auth_password(
            &mut self,
            user: &str,
            password: &str,
        ) -> Result<russh::server::Auth, Self::Error> {
            if user == "test" && password == "test123" {
                Ok(russh::server::Auth::Accept)
            } else {
                Ok(russh::server::Auth::reject())
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
            .filter_level(log::LevelFilter::Debug)
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
                Err(_) => {}
            }
        }
        panic!("timeout. output:\n{output}");
    }
}
