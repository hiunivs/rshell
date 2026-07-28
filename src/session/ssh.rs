//! SSH 会话:russh 建立连接,独立线程跑 current_thread tokio runtime。
//! shell 通道的字流与本地 pty 一样经 SessionEvent/SessionCmd 与 UI 交互。

use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use russh::ChannelMsg;
use russh::client::{self, Config, Handler};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, load_secret_key};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::files::{FileEntry, SftpCmd, SftpEvent, SftpRx, sftp_channel};
use super::monitor::{MONITOR_CMD, MonitorState};
use super::{
    CmdRx, HostKeyPrompt, HostKeyVerdict, SessionChannels, SessionCmd, SessionEvent, cmd_channel,
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
    let (event_tx, event_rx) = channel::<SessionEvent>();
    let (sftp_tx, sftp_rx) = sftp_channel();
    let sftp_tx_ret = sftp_tx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(run(params, cols, rows, cmd_rx, sftp_rx, event_tx, ctx));
    });
    SessionChannels {
        cmd_tx,
        event_rx,
        sftp_tx: sftp_tx_ret,
    }
}

/// 服务器指纹校验:known_hosts 记录命中则直接通过;
/// 新主机/指纹变更时向 UI 发确认请求并阻塞等待用户决定(TOFU)。
struct ClientHandler {
    host: String,
    port: u16,
    event_tx: Sender<SessionEvent>,
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
                            let _ = self.event_tx.send(SessionEvent::Data(ansi(&format!(
                                "\x1b[1;31m保存主机指纹失败：{error:#}\x1b[0m\r\n"
                            ))));
                            self.ctx.request_repaint();
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

async fn run(
    params: SshParams,
    cols: u16,
    rows: u16,
    mut cmd_rx: CmdRx,
    sftp_rx: SftpRx,
    event_tx: Sender<SessionEvent>,
    ctx: egui::Context,
) {
    // 进度与错误直接打印到终端区
    macro_rules! status {
        ($($arg:tt)*) => {{
            let _ = event_tx.send(SessionEvent::Data(ansi(&format!("\x1b[36m{}\x1b[0m\r\n", format!($($arg)*)))));
            ctx.request_repaint();
        }};
    }
    macro_rules! fatal {
        ($($arg:tt)*) => {{
            let _ = event_tx.send(SessionEvent::Data(ansi(&format!("\x1b[1;31m{}\x1b[0m\r\n", format!($($arg)*)))));
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
            command = cmd_rx.recv() => match command {
                Some(SessionCmd::Shutdown) | None => {
                    let _ = event_tx.send(SessionEvent::Exited);
                    ctx.request_repaint();
                    return;
                }
                Some(SessionCmd::Input(_)) | Some(SessionCmd::Resize { .. }) => {}
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

    // ---- shell 通道 ----
    let handle = Arc::new(handle);
    let mut channel = match handle.channel_open_session().await {
        Ok(c) => c,
        Err(e) => fatal!("打开通道失败:{e}"),
    };
    if let Err(e) = channel
        .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await
    {
        fatal!("请求 PTY 失败:{e}");
    }
    if let Err(e) = channel.request_shell(false).await {
        fatal!("启动 shell 失败:{e}");
    }

    // ---- 监控任务:同一连接上另开 exec 通道,1s 一轮 ----
    // shutdown 标志:主动断开时主循环置位,监控循环随之退出
    // (否则它持有的 Arc<Handle> 会让连接永远断不掉)
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mon_handle = handle.clone();
    let mon_tx = event_tx.clone();
    let mon_ctx = ctx.clone();
    let mon_shutdown = shutdown.clone();
    tokio::spawn(async move {
        monitor_loop(mon_handle, mon_tx, mon_ctx, mon_shutdown).await;
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
                        if event_tx.send(SessionEvent::Data(data.to_vec())).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if event_tx.send(SessionEvent::Data(data.to_vec())).is_err() {
                            break;
                        }
                        ctx.request_repaint();
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

/// 每秒通过一条 exec 命令采集系统指标;shutdown 置位或连接断开即退出
async fn monitor_loop(
    handle: Arc<client::Handle<ClientHandler>>,
    event_tx: Sender<SessionEvent>,
    ctx: egui::Context,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let mut state = MonitorState::default();
    let mut consecutive_errors = 0u32;
    // 等 shell 先起来
    tokio::time::sleep(Duration::from_millis(500)).await;
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let t0 = Instant::now();
        match run_exec(&handle, MONITOR_CMD).await {
            Ok(text) => {
                if consecutive_errors > 0 {
                    let _ = event_tx.send(SessionEvent::Data(ansi(
                        "\x1b[36m系统监控已恢复\x1b[0m\r\n",
                    )));
                }
                consecutive_errors = 0;
                let rtt = t0.elapsed().as_millis() as u32;
                let metrics = state.update(&text, rtt);
                if event_tx
                    .send(SessionEvent::Metrics(Box::new(metrics)))
                    .is_err()
                {
                    break;
                }
                ctx.request_repaint();
            }
            Err(error) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                eprintln!("SSH 监控采样失败（第 {consecutive_errors} 次，将自动重试）：{error}");
                if consecutive_errors == 1 {
                    let _ = event_tx.send(SessionEvent::Data(ansi(
                        "\x1b[33m系统监控暂时不可用，正在自动重试\x1b[0m\r\n",
                    )));
                    ctx.request_repaint();
                }
                let delay = 1u64 << consecutive_errors.min(5);
                tokio::time::sleep(Duration::from_secs(delay)).await;
                continue;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
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
        Ok(s) => s,
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

    while let Some(cmd) = rx.recv().await {
        match cmd {
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
            SftpCmd::Download { id, remote, local } => {
                let r = download(&sftp, id, &remote, &local, &event_tx, &ctx).await;
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::OpDone {
                    id: Some(id),
                    op: "download",
                    result: r,
                }));
            }
            SftpCmd::DownloadDir { id, remote, local } => {
                let r = download_dir(&sftp, id, &remote, &local, &event_tx, &ctx).await;
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::OpDone {
                    id: Some(id),
                    op: "download_dir",
                    result: r,
                }));
            }
            SftpCmd::UploadDir { id, local, remote } => {
                let r = upload_dir(&sftp, id, &local, &remote, &event_tx, &ctx).await;
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::OpDone {
                    id: Some(id),
                    op: "upload_dir",
                    result: r,
                }));
            }
            SftpCmd::Upload { id, local, remote } => {
                let r = upload(&sftp, id, &local, &remote, &event_tx, &ctx).await;
                let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::OpDone {
                    id: Some(id),
                    op: "upload",
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
        }
        ctx.request_repaint();
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

/// 遍历远程目录,收集(相对路径, 文件大小)
async fn walk_remote(
    sftp: &SftpSession,
    base: &str,
    rel: String,
    out: &mut Vec<(String, u64)>,
) -> Result<(), String> {
    let path = if rel.is_empty() {
        base.to_string()
    } else {
        super::files::join_path(base, &rel)
    };
    let rd = sftp.read_dir(&path).await.map_err(|e| e.to_string())?;
    for e in rd {
        let name = e.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let meta = e.metadata();
        if e.file_type().is_dir() {
            Box::pin(walk_remote(sftp, base, child, out)).await?;
        } else {
            out.push((child, meta.size.unwrap_or(0)));
        }
    }
    Ok(())
}

/// 远程→本地目录:先遍历收集,再逐文件下载
async fn download_dir(
    sftp: &SftpSession,
    id: u64,
    remote: &str,
    local: &str,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let mut files = Vec::new();
    walk_remote(sftp, remote, String::new(), &mut files).await?;
    let total: u64 = files.iter().map(|(_, s)| *s).sum();
    let mut done = 0u64;
    for (rel, _size) in files {
        let r = super::files::join_path(remote, &rel);
        let l = std::path::Path::new(local).join(&rel);
        if let Some(parent) = l.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {}: {e}", parent.display()))?;
        }
        let l_s = l.to_string_lossy().into_owned();
        // 逐块下载
        let mut src = sftp.open(&r).await.map_err(|e| e.to_string())?;
        let mut dst = std::fs::File::create(&l_s).map_err(|e| format!("无法创建 {l_s}: {e}"))?;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            use tokio::io::AsyncReadExt;
            let n = src.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            use std::io::Write;
            dst.write_all(&buf[..n])
                .map_err(|e| format!("写入 {l_s} 失败: {e}"))?;
            done += n as u64;
            let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::Progress {
                id,
                op: "download_dir",
                path: remote.to_string(),
                done,
                total,
            }));
            ctx.request_repaint();
        }
    }
    Ok(())
}

/// 本地→远程目录:遍历本地收集,逐文件上传
async fn upload_dir(
    sftp: &SftpSession,
    id: u64,
    local: &str,
    remote: &str,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let mut files = Vec::new();
    super::files::walk_files(local, String::new(), &mut files).map_err(|e| e.to_string())?;
    let total: u64 = files.iter().map(|(_, s)| *s).sum();
    let mut done = 0u64;
    ensure_remote_dir_all(sftp, remote).await?;
    for (rel, _size) in files {
        let l = std::path::Path::new(local).join(&rel);
        let r = super::files::join_path(remote, &rel);
        if let Some(parent) = r.rfind('/') {
            ensure_remote_dir_all(sftp, &r[..parent]).await?;
        }
        let l_s = l.to_string_lossy().into_owned();
        let mut src = std::fs::File::open(&l_s).map_err(|e| format!("无法打开 {l_s}: {e}"))?;
        let mut dst = sftp.create(&r).await.map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            use std::io::Read;
            let n = src
                .read(&mut buf)
                .map_err(|e| format!("读取 {l_s} 失败: {e}"))?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n])
                .await
                .map_err(|e| format!("写入 {r} 失败: {e}"))?;
            done += n as u64;
            let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::Progress {
                id,
                op: "upload_dir",
                path: local.to_string(),
                done,
                total,
            }));
            ctx.request_repaint();
        }
        dst.shutdown()
            .await
            .map_err(|e| format!("关闭 {r} 失败: {e}"))?;
    }
    Ok(())
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
    sftp: &SftpSession,
    id: u64,
    remote: &str,
    local: &str,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    use std::io::Write as _;
    let total = sftp
        .metadata(remote)
        .await
        .ok()
        .and_then(|m| m.size)
        .unwrap_or(0);
    let mut src = sftp
        .open(remote)
        .await
        .map_err(|e| format!("无法打开远程文件 {remote}: {e}"))?;
    let mut dst =
        std::fs::File::create(local).map_err(|e| format!("无法创建本地文件 {local}: {e}"))?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = src
            .read(&mut buf)
            .await
            .map_err(|e| format!("读取远程文件 {remote} 失败: {e}"))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .map_err(|e| format!("写入本地文件 {local} 失败: {e}"))?;
        done += n as u64;
        let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::Progress {
            id,
            op: "download",
            path: remote.to_string(),
            done,
            total,
        }));
        ctx.request_repaint();
    }
    Ok(())
}

/// 本地 → 远程
async fn upload(
    sftp: &SftpSession,
    id: u64,
    local: &str,
    remote: &str,
    event_tx: &Sender<SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    use std::io::Read as _;
    let total = std::fs::metadata(local)
        .map_err(|e| format!("无法读取本地文件 {local}: {e}"))?
        .len();
    let mut src =
        std::fs::File::open(local).map_err(|e| format!("无法打开本地文件 {local}: {e}"))?;
    let mut dst = sftp
        .create(remote)
        .await
        .map_err(|e| format!("无法创建远程文件 {remote}: {e}"))?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = src
            .read(&mut buf)
            .map_err(|e| format!("读取本地文件 {local} 失败: {e}"))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])
            .await
            .map_err(|e| format!("写入远程文件 {remote} 失败: {e}"))?;
        done += n as u64;
        let _ = event_tx.send(SessionEvent::Sftp(SftpEvent::Progress {
            id,
            op: "upload",
            path: local.to_string(),
            done,
            total,
        }));
        ctx.request_repaint();
    }
    dst.shutdown()
        .await
        .map_err(|e| format!("关闭远程文件 {remote} 失败: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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
        let ch = spawn(
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
            match ch.event_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(SessionEvent::Data(data)) => {
                    output.push_str(&String::from_utf8_lossy(&data));
                    if !greeted && output.contains("RSHELL_SRV_GREETING") {
                        greeted = true;
                        ch.cmd_tx
                            .send(SessionCmd::Input(b"hello_rshell".to_vec()))
                            .unwrap();
                    }
                    if greeted && output.contains("echo:hello_rshell") {
                        return; // 双向数据验证通过
                    }
                }
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
