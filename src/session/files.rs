//! 文件管理共享模型:FileEntry、SFTP 命令/事件、路径与时间工具。
//! 远程会话经 russh-sftp 实现,本机会话经 std::fs 实现,UI 统一消费。

use std::sync::atomic::{AtomicBool, Ordering};

type LocalTransferControls = std::sync::Arc<
    std::sync::Mutex<
        std::collections::HashMap<u64, (std::sync::Arc<TransferControl>, &'static str)>,
    >,
>;

#[derive(Default)]
pub(crate) struct TransferControl {
    canceled: AtomicBool,
    paused: AtomicBool,
    started: AtomicBool,
}

impl TransferControl {
    pub(crate) fn cancel(&self) {
        self.canceled.store(true, Ordering::Release);
        self.paused.store(false, Ordering::Release);
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    fn mark_started(&self) {
        self.started.store(true, Ordering::Release);
    }

    fn has_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    pub(crate) fn wait_blocking(&self) -> Result<(), String> {
        while self.paused.load(Ordering::Acquire) {
            if self.canceled.load(Ordering::Acquire) {
                return Err("传输已取消".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if self.canceled.load(Ordering::Acquire) {
            Err("传输已取消".into())
        } else {
            Ok(())
        }
    }

    pub(crate) async fn wait_async(&self) -> Result<(), String> {
        while self.paused.load(Ordering::Acquire) {
            if self.canceled.load(Ordering::Acquire) {
                return Err("传输已取消".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        if self.canceled.load(Ordering::Acquire) {
            Err("传输已取消".into())
        } else {
            Ok(())
        }
    }

    pub(crate) async fn wait_canceled(&self) {
        while !self.canceled.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

/// 一个目录项
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// unix 秒,0 = 未知
    pub mtime: i64,
    /// unix 权限位(含文件类型位)
    pub permissions: u32,
    pub uid: u32,
    pub gid: u32,
    pub owner: String,
    pub group: String,
    /// 是符号链接(is_dir/size 等已按链接目标解析)
    pub is_symlink: bool,
}

impl FileEntry {
    pub fn full_path(&self, dir: &str) -> String {
        join_path(dir, &self.name)
    }

    /// "drwxr-xr-x" 风格
    pub fn perm_string(&self) -> String {
        format_mode(self.permissions, self.is_dir)
    }

    pub fn mtime_string(&self) -> String {
        if self.mtime <= 0 {
            return "—".into();
        }
        format_timestamp(self.mtime)
    }
}

/// UI → 文件服务的命令
pub enum SftpCmd {
    List(String),
    Mkdir(String),
    Remove {
        path: String,
        is_dir: bool,
    },
    /// 递归删除目录及其所有内容(非空目录用)
    RemoveRecursive {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Download {
        id: u64,
        remote: String,
        local: String,
    },
    /// 递归上传整个目录到远端
    UploadDir {
        id: u64,
        local: String,
        remote: String,
    },
    /// 递归下载整个目录到本地
    DownloadDir {
        id: u64,
        remote: String,
        local: String,
    },
    Upload {
        id: u64,
        local: String,
        remote: String,
    },
    CancelTransfer {
        id: u64,
    },
    PauseTransfer {
        id: u64,
    },
    ResumeTransfer {
        id: u64,
    },
    /// 设置 unix 权限位(含文件类型位,直接传给 setstat)
    SetPermissions {
        path: String,
        mode: u32,
    },
    /// 读取文件内容到内存(最多 max_bytes 字节;供在线预览/编辑)
    ReadFile {
        id: u64,
        path: String,
        max_bytes: u64,
    },
    /// 把内存中的内容整体写回文件(编辑器保存;远端先写临时文件再 rename)
    WriteFile {
        id: u64,
        path: String,
        data: Vec<u8>,
    },
    /// 用 `file --mime-type` 判定文件类型(层级 3;不支持的环境返回 Err)
    DetectKind {
        id: u64,
        path: String,
    },
}

/// 文件服务 → UI 的事件
pub enum SftpEvent {
    /// 目录列表结果
    Listed {
        path: String,
        result: Result<Vec<FileEntry>, String>,
    },
    /// 文件服务初始化结果(仅失败时上报)
    SetupFailed(String),
    /// 会话默认目录(通常是用户主目录),连接建立后上报一次
    HomeDir(String),
    /// mkdir/remove/rename/download/upload 完成
    OpDone {
        /// 传输任务 ID；普通文件操作没有 ID。
        id: Option<u64>,
        op: &'static str,
        result: Result<(), String>,
    },
    /// 传输进度
    Progress {
        id: u64,
        op: &'static str,
        path: String,
        done: u64,
        total: u64,
    },
    /// ReadFile 的结果(在线预览/编辑内容)
    FileContent {
        id: u64,
        path: String,
        result: Result<Vec<u8>, String>,
    },
    /// DetectKind 的结果(`file --mime-type -b` 的原始输出,已 trim)
    KindDetected {
        id: u64,
        path: String,
        result: Result<String, String>,
    },
}

pub type SftpTx = tokio::sync::mpsc::Sender<SftpCmd>;
pub type SftpRx = tokio::sync::mpsc::Receiver<SftpCmd>;

pub fn sftp_channel() -> (SftpTx, SftpRx) {
    tokio::sync::mpsc::channel(128)
}

// ---------------------------------------------------------------------------
// 路径工具(统一用 '/' 分隔)
// ---------------------------------------------------------------------------

pub fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// 规范化绝对路径:处理 . 、..、多余斜杠。非绝对路径按 "/"+path 处理。
pub fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        "/".into()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// 上级目录
pub fn parent_path(path: &str) -> String {
    let norm = normalize_path(path);
    if norm == "/" {
        return "/".into();
    }
    match norm.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => norm[..i].to_string(),
    }
}

// ---------------------------------------------------------------------------
// 显示格式化
// ---------------------------------------------------------------------------

/// unix mode → "drwxr-xr-x"
pub fn format_mode(mode: u32, is_dir: bool) -> String {
    let mut s = String::with_capacity(10);
    s.push(if is_dir { 'd' } else { '-' });
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// unix 秒 → "2025/08/01 22:37"(本地时区)
pub fn format_timestamp(ts: i64) -> String {
    // 本地时区偏移:优先读 TZ 环境,简化处理用系统偏移
    let local = ts + timezone_offset_secs(ts);
    let days = local.div_euclid(86400);
    let secs = local.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}/{:02}/{:02} {:02}:{:02}",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// 天数 → 公历年月日(Howard Hinnant 算法)
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 估算本地时区偏移(秒);缓存,只算一次
fn timezone_offset_secs(_ts: i64) -> i64 {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| local_offset_minutes() * 60)
}

/// 借助 `date +%z` 获取本地时区(分钟);失败时按 UTC
#[cfg(unix)]
fn local_offset_minutes() -> i64 {
    std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            let s = s.trim().to_string();
            let sign = if s.starts_with('-') { -1 } else { 1 };
            let digits: String = s.chars().skip(1).take(4).collect();
            let v: i64 = digits.parse().ok()?;
            Some(sign * (v / 100 * 60 + v % 100))
        })
        .unwrap_or(0)
}

#[cfg(not(unix))]
fn local_offset_minutes() -> i64 {
    0
}

// ---------------------------------------------------------------------------
// 本机文件服务(std::fs,独立线程)
// ---------------------------------------------------------------------------

/// 启动本机文件服务线程,事件经 SessionEvent 通道送回
pub fn spawn_local_worker(
    mut rx: SftpRx,
    event_tx: std::sync::mpsc::Sender<super::SessionEvent>,
    ctx: egui::Context,
) {
    use std::sync::{Arc, Mutex};

    let controls: LocalTransferControls = Arc::new(Mutex::new(Default::default()));
    let (work_tx, work_rx) = std::sync::mpsc::sync_channel::<(SftpCmd, Arc<TransferControl>)>(32);
    let work_rx = Arc::new(Mutex::new(work_rx));
    for _ in 0..2 {
        let worker_rx = Arc::clone(&work_rx);
        let worker_tx = event_tx.clone();
        let worker_ctx = ctx.clone();
        let worker_controls = Arc::clone(&controls);
        std::thread::spawn(move || {
            loop {
                let task = worker_rx.lock().expect("local transfer queue").recv();
                let Ok((cmd, control)) = task else {
                    break;
                };
                let id = transfer_command_id(&cmd);
                control.mark_started();
                let event = handle_local_cmd_with_ctx(cmd, &worker_tx, &worker_ctx, &control);
                let _ = worker_tx.send(super::SessionEvent::Sftp(event));
                worker_controls
                    .lock()
                    .expect("local transfer controls")
                    .remove(&id);
                worker_ctx.request_repaint();
            }
        });
    }

    std::thread::spawn(move || {
        while let Some(cmd) = rx.blocking_recv() {
            match cmd {
                SftpCmd::CancelTransfer { id } => {
                    let queued = controls
                        .lock()
                        .expect("local transfer controls")
                        .get(&id)
                        .map(|(control, op)| (Arc::clone(control), *op));
                    if let Some((control, op)) = queued {
                        control.cancel();
                        if !control.has_started() {
                            let _ = event_tx.send(super::SessionEvent::Sftp(SftpEvent::OpDone {
                                id: Some(id),
                                op,
                                result: Err("传输已取消".into()),
                            }));
                            ctx.request_repaint();
                        }
                    }
                    continue;
                }
                SftpCmd::PauseTransfer { id } => {
                    if let Some((control, _)) =
                        controls.lock().expect("local transfer controls").get(&id)
                    {
                        control.set_paused(true);
                    }
                    continue;
                }
                SftpCmd::ResumeTransfer { id } => {
                    if let Some((control, _)) =
                        controls.lock().expect("local transfer controls").get(&id)
                    {
                        control.set_paused(false);
                    }
                    continue;
                }
                cmd @ (SftpCmd::Download { .. }
                | SftpCmd::UploadDir { .. }
                | SftpCmd::DownloadDir { .. }
                | SftpCmd::Upload { .. }) => {
                    let id = transfer_command_id(&cmd);
                    let control = Arc::new(TransferControl::default());
                    controls
                        .lock()
                        .expect("local transfer controls")
                        .insert(id, (Arc::clone(&control), transfer_command_op(&cmd)));
                    if let Err(error) = work_tx.try_send((cmd, control)) {
                        controls
                            .lock()
                            .expect("local transfer controls")
                            .remove(&id);
                        let (cmd, _) = match error {
                            std::sync::mpsc::TrySendError::Full(task)
                            | std::sync::mpsc::TrySendError::Disconnected(task) => task,
                        };
                        let _ = event_tx.send(super::SessionEvent::Sftp(transfer_failed_event(
                            cmd,
                            "本机传输队列已满",
                        )));
                    }
                    continue;
                }
                cmd => {
                    let control = TransferControl::default();
                    let ev = handle_local_cmd_with_ctx(cmd, &event_tx, &ctx, &control);
                    let _ = event_tx.send(super::SessionEvent::Sftp(ev));
                    ctx.request_repaint();
                }
            }
        }
    });
}

fn handle_local_cmd_with_ctx(
    cmd: SftpCmd,
    event_tx: &std::sync::mpsc::Sender<super::SessionEvent>,
    ctx: &egui::Context,
    control: &TransferControl,
) -> SftpEvent {
    match cmd {
        SftpCmd::List(path) => {
            let result = list_local(&path).map_err(|e| format!("{e}"));
            SftpEvent::Listed { path, result }
        }
        SftpCmd::Mkdir(path) => SftpEvent::OpDone {
            id: None,
            op: "mkdir",
            result: std::fs::create_dir(&path).map_err(|e| e.to_string()),
        },
        SftpCmd::Remove { path, is_dir } => SftpEvent::OpDone {
            id: None,
            op: "remove",
            result: if is_dir {
                std::fs::remove_dir(&path).map_err(|e| e.to_string())
            } else {
                std::fs::remove_file(&path).map_err(|e| e.to_string())
            },
        },
        SftpCmd::RemoveRecursive { path } => SftpEvent::OpDone {
            id: None,
            op: "remove",
            result: remove_local_recursive(&path),
        },
        SftpCmd::Rename { from, to } => SftpEvent::OpDone {
            id: None,
            op: "rename",
            result: std::fs::rename(&from, &to).map_err(|e| e.to_string()),
        },
        SftpCmd::Download { id, remote, local } => {
            copy_with_progress(id, &remote, &local, "download", event_tx, ctx, control)
        }
        SftpCmd::UploadDir { id, local, remote } => {
            let result =
                copy_dir_with_progress(id, &local, &remote, "upload_dir", event_tx, ctx, control);
            SftpEvent::OpDone {
                id: Some(id),
                op: "upload_dir",
                result,
            }
        }
        SftpCmd::DownloadDir { id, remote, local } => {
            let result =
                copy_dir_with_progress(id, &remote, &local, "download_dir", event_tx, ctx, control);
            SftpEvent::OpDone {
                id: Some(id),
                op: "download_dir",
                result,
            }
        }
        SftpCmd::Upload { id, local, remote } => {
            copy_with_progress(id, &local, &remote, "upload", event_tx, ctx, control)
        }
        SftpCmd::SetPermissions { path, mode } => SftpEvent::OpDone {
            id: None,
            op: "chmod",
            result: set_local_perms(&path, mode),
        },
        SftpCmd::ReadFile {
            id,
            path,
            max_bytes,
        } => {
            let result = read_local_capped(&path, max_bytes);
            SftpEvent::FileContent { id, path, result }
        }
        SftpCmd::WriteFile { id, path, data } => SftpEvent::OpDone {
            id: Some(id),
            op: "write",
            result: crate::storage::atomic_write(std::path::Path::new(&path), &data, false)
                .map_err(|e| format!("{e:#}")),
        },
        SftpCmd::DetectKind { id, path } => {
            let result = detect_kind_local(&path);
            SftpEvent::KindDetected { id, path, result }
        }
        SftpCmd::CancelTransfer { .. }
        | SftpCmd::PauseTransfer { .. }
        | SftpCmd::ResumeTransfer { .. } => SftpEvent::SetupFailed("无效的传输控制命令".into()),
    }
}

/// 读取本机文件前 max_bytes 字节(供在线预览/编辑)
fn read_local_capped(path: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| format!("无法打开 {path}: {e}"))?;
    let mut out = Vec::new();
    file.take(max_bytes)
        .read_to_end(&mut out)
        .map_err(|e| format!("读取 {path} 失败: {e}"))?;
    Ok(out)
}

/// 本机 `file --mime-type -b`;平台没有 file 命令时返回 Err(调用方回退)
#[cfg(unix)]
fn detect_kind_local(path: &str) -> Result<String, String> {
    let out = std::process::Command::new("file")
        .args(["--mime-type", "-b", "--", path])
        .output()
        .map_err(|e| format!("无法执行 file 命令: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "file 命令失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(not(unix))]
fn detect_kind_local(_path: &str) -> Result<String, String> {
    Err("当前平台不支持 file 命令".into())
}

#[cfg(test)]
fn handle_local_cmd(
    cmd: SftpCmd,
    event_tx: &std::sync::mpsc::Sender<super::SessionEvent>,
) -> SftpEvent {
    handle_local_cmd_with_ctx(
        cmd,
        event_tx,
        &egui::Context::default(),
        &TransferControl::default(),
    )
}

fn transfer_command_id(cmd: &SftpCmd) -> u64 {
    match cmd {
        SftpCmd::Download { id, .. }
        | SftpCmd::UploadDir { id, .. }
        | SftpCmd::DownloadDir { id, .. }
        | SftpCmd::Upload { id, .. } => *id,
        _ => unreachable!("not a transfer command"),
    }
}

fn transfer_command_op(cmd: &SftpCmd) -> &'static str {
    match cmd {
        SftpCmd::Download { .. } => "download",
        SftpCmd::UploadDir { .. } => "upload_dir",
        SftpCmd::DownloadDir { .. } => "download_dir",
        SftpCmd::Upload { .. } => "upload",
        _ => unreachable!("not a transfer command"),
    }
}

fn transfer_failed_event(cmd: SftpCmd, message: &str) -> SftpEvent {
    let id = transfer_command_id(&cmd);
    let op = transfer_command_op(&cmd);
    SftpEvent::OpDone {
        id: Some(id),
        op,
        result: Err(message.into()),
    }
}

/// 递归拷贝目录：边遍历边传输，不保存完整文件清单。
fn copy_dir_with_progress(
    id: u64,
    src_base: &str,
    dst_base: &str,
    op: &'static str,
    event_tx: &std::sync::mpsc::Sender<super::SessionEvent>,
    ctx: &egui::Context,
    control: &TransferControl,
) -> Result<(), String> {
    let mut done = 0u64;
    let mut last_progress = std::time::Instant::now() - std::time::Duration::from_millis(100);
    std::fs::create_dir_all(dst_base).map_err(|e| format!("创建目录 {dst_base} 失败: {e}"))?;
    copy_dir_stream(
        id,
        std::path::Path::new(src_base),
        std::path::Path::new(dst_base),
        op,
        src_base,
        event_tx,
        ctx,
        control,
        &mut done,
        &mut last_progress,
    )
}

#[allow(clippy::too_many_arguments)]
fn copy_dir_stream(
    id: u64,
    src_dir: &std::path::Path,
    dst_dir: &std::path::Path,
    op: &'static str,
    progress_path: &str,
    event_tx: &std::sync::mpsc::Sender<super::SessionEvent>,
    ctx: &egui::Context,
    control: &TransferControl,
    done: &mut u64,
    last_progress: &mut std::time::Instant,
) -> Result<(), String> {
    control.wait_blocking()?;
    for entry in std::fs::read_dir(src_dir)
        .map_err(|e| format!("读取目录 {} 失败: {e}", src_dir.display()))?
    {
        control.wait_blocking()?;
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_symlink() {
            return Err(format!(
                "目录传输暂不跟随符号链接，已停止于：{}",
                entry.path().display()
            ));
        }
        let metadata = entry.metadata().map_err(|e| e.to_string())?;
        let src = entry.path();
        let dst = dst_dir.join(entry.file_name());
        if metadata.is_dir() {
            std::fs::create_dir_all(&dst)
                .map_err(|e| format!("创建目录 {} 失败: {e}", dst.display()))?;
            copy_dir_stream(
                id,
                &src,
                &dst,
                op,
                progress_path,
                event_tx,
                ctx,
                control,
                done,
                last_progress,
            )?;
        } else {
            let source_tag = local_source_tag(&metadata);
            copy_dir_file(
                id,
                &src,
                &dst,
                metadata.len(),
                &source_tag,
                op,
                progress_path,
                event_tx,
                ctx,
                control,
                done,
                last_progress,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_dir_file(
    id: u64,
    src: &std::path::Path,
    dst: &std::path::Path,
    expected: u64,
    source_tag: &str,
    op: &'static str,
    progress_path: &str,
    event_tx: &std::sync::mpsc::Sender<super::SessionEvent>,
    ctx: &egui::Context,
    control: &TransferControl,
    done: &mut u64,
    last_progress: &mut std::time::Instant,
) -> Result<(), String> {
    let partial = local_partial_path(dst);
    let resume = valid_local_resume(&partial, expected, source_tag)?;
    let mut src_file =
        std::fs::File::open(src).map_err(|e| format!("无法读取 {}: {e}", src.display()))?;
    let mut dst_file = open_local_partial(&partial, resume)?;
    use std::io::{Read, Seek, Write};
    src_file
        .seek(std::io::SeekFrom::Start(resume))
        .map_err(|e| e.to_string())?;
    *done = done.saturating_add(resume);
    let mut file_done = resume;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        control.wait_blocking()?;
        let n = src_file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        dst_file
            .write_all(&buf[..n])
            .map_err(|e| format!("写入 {} 失败: {e}", partial.display()))?;
        file_done += n as u64;
        *done += n as u64;
        send_progress(
            event_tx,
            ctx,
            last_progress,
            (id, op, progress_path, *done, 0),
        );
    }
    if file_done != expected {
        return Err(format!(
            "复制 {} 不完整：预期 {expected} 字节，实际 {file_done} 字节",
            src.display()
        ));
    }
    let final_metadata = std::fs::metadata(src).map_err(|e| e.to_string())?;
    if local_source_tag(&final_metadata) != source_tag {
        return Err(format!("复制期间源文件 {} 发生变化", src.display()));
    }
    control.wait_blocking()?;
    dst_file.sync_all().map_err(|e| e.to_string())?;
    drop(dst_file);
    commit_local_partial(&partial, dst)
}

/// 递归删除:目录用 remove_dir_all,文件/链接用 remove_file
fn remove_local_recursive(path: &str) -> Result<(), String> {
    let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    if is_dir {
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(path).map_err(|e| e.to_string())
    }
}

#[cfg(unix)]
fn set_local_perms(path: &str, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn set_local_perms(_path: &str, _mode: u32) -> Result<(), String> {
    Err("本机不支持设置权限".into())
}

fn copy_with_progress(
    id: u64,
    from: &str,
    to: &str,
    op: &'static str,
    event_tx: &std::sync::mpsc::Sender<super::SessionEvent>,
    ctx: &egui::Context,
    control: &TransferControl,
) -> SftpEvent {
    use std::io::{Read, Seek, Write};
    let result = (|| -> Result<(), String> {
        let mut src = std::fs::File::open(from).map_err(|e| format!("无法读取 {from}: {e}"))?;
        let total = src.metadata().map_err(|e| e.to_string())?.len();
        let source_tag = local_source_tag(&src.metadata().map_err(|e| e.to_string())?);
        let partial = local_partial_path(std::path::Path::new(to));
        let resume = valid_local_resume(&partial, total, &source_tag)?;
        src.seek(std::io::SeekFrom::Start(resume))
            .map_err(|e| format!("定位 {from} 失败: {e}"))?;
        let mut dst = open_local_partial(&partial, resume)?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = resume;
        let mut last_progress = std::time::Instant::now() - std::time::Duration::from_millis(100);
        send_progress(
            event_tx,
            ctx,
            &mut last_progress,
            (id, op, from, done, total),
        );
        loop {
            control.wait_blocking()?;
            let n = src
                .read(&mut buf)
                .map_err(|e| format!("读取 {from} 失败: {e}"))?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n])
                .map_err(|e| format!("写入 {to} 失败: {e}"))?;
            done += n as u64;
            send_progress(
                event_tx,
                ctx,
                &mut last_progress,
                (id, op, from, done, total),
            );
        }
        if done != total {
            return Err(format!(
                "复制不完整：预期 {total} 字节，实际写入 {done} 字节"
            ));
        }
        if local_source_tag(&src.metadata().map_err(|e| e.to_string())?) != source_tag {
            return Err("复制期间源文件发生变化".into());
        }
        control.wait_blocking()?;
        dst.sync_all().map_err(|e| e.to_string())?;
        drop(dst);
        commit_local_partial(&partial, std::path::Path::new(to))
    })();
    SftpEvent::OpDone {
        id: Some(id),
        op,
        result,
    }
}

fn local_partial_path(target: &std::path::Path) -> std::path::PathBuf {
    let mut value = target.as_os_str().to_os_string();
    value.push(".rshell-part");
    value.into()
}

fn valid_local_resume(
    partial: &std::path::Path,
    total: u64,
    source_tag: &str,
) -> Result<u64, String> {
    let meta_path = local_partial_meta_path(partial);
    let stored_tag = std::fs::read_to_string(&meta_path).unwrap_or_default();
    let size = std::fs::metadata(partial)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if stored_tag.trim() == source_tag && size <= total {
        return Ok(size);
    }
    let _ = std::fs::remove_file(partial);
    std::fs::write(&meta_path, source_tag)
        .map_err(|e| format!("写入断点信息 {} 失败: {e}", meta_path.display()))?;
    Ok(0)
}

fn local_partial_meta_path(partial: &std::path::Path) -> std::path::PathBuf {
    let mut value = partial.as_os_str().to_os_string();
    value.push(".meta");
    value.into()
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

fn commit_local_partial(partial: &std::path::Path, target: &std::path::Path) -> Result<(), String> {
    std::fs::rename(partial, target)
        .map_err(|e| format!("提交文件 {} 失败: {e}", target.display()))?;
    let _ = std::fs::remove_file(local_partial_meta_path(partial));
    Ok(())
}

fn open_local_partial(path: &std::path::Path, offset: u64) -> Result<std::fs::File, String> {
    use std::io::{Seek, Write};
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("创建临时文件 {} 失败: {e}", path.display()))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(|e| format!("定位临时文件 {} 失败: {e}", path.display()))?;
    if offset == 0 {
        file.set_len(0)
            .map_err(|e| format!("重置临时文件 {} 失败: {e}", path.display()))?;
        file.flush().map_err(|e| e.to_string())?;
    }
    Ok(file)
}

fn send_progress(
    event_tx: &std::sync::mpsc::Sender<super::SessionEvent>,
    ctx: &egui::Context,
    last: &mut std::time::Instant,
    (id, op, path, done, total): (u64, &'static str, &str, u64, u64),
) {
    if last.elapsed() < std::time::Duration::from_millis(100) {
        return;
    }
    let _ = event_tx.send(super::SessionEvent::Sftp(SftpEvent::Progress {
        id,
        op,
        path: path.to_string(),
        done,
        total,
    }));
    *last = std::time::Instant::now();
    ctx.request_repaint();
}

fn list_local(path: &str) -> std::io::Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // DirEntry::metadata 不跟随符号链接,需用 fs::metadata 解析链接目标;
        // 链接目标不存在(悬空链接)时退化为链接自身的属性
        let meta = std::fs::metadata(entry.path()).or_else(|_| entry.metadata())?;
        let is_symlink = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        out.push(local_entry(name, &meta, is_symlink));
    }
    Ok(out)
}

#[cfg(unix)]
fn local_entry(name: String, meta: &std::fs::Metadata, is_symlink: bool) -> FileEntry {
    use std::os::unix::fs::MetadataExt;
    FileEntry {
        name,
        is_dir: meta.is_dir(),
        size: meta.len(),
        mtime: meta.mtime(),
        permissions: meta.mode(),
        uid: meta.uid(),
        gid: meta.gid(),
        owner: String::new(),
        group: String::new(),
        is_symlink,
    }
}

#[cfg(not(unix))]
fn local_entry(name: String, meta: &std::fs::Metadata, is_symlink: bool) -> FileEntry {
    use std::time::UNIX_EPOCH;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    FileEntry {
        name,
        is_dir: meta.is_dir(),
        size: meta.len(),
        mtime,
        permissions: 0o755,
        uid: 0,
        gid: 0,
        owner: String::new(),
        group: String::new(),
        is_symlink,
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_local_capped_reads_prefix() {
        let dir = std::env::temp_dir().join(format!("rshell-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cap.txt");
        std::fs::write(&path, b"0123456789abcdef").unwrap();
        let full = read_local_capped(path.to_str().unwrap(), 1024).unwrap();
        assert_eq!(full, b"0123456789abcdef");
        let prefix = read_local_capped(path.to_str().unwrap(), 6).unwrap();
        assert_eq!(prefix, b"012345");
        assert!(read_local_capped("/nonexistent/rshell-x", 10).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 本机 file 命令判定(层级 3;依赖系统 file,macOS/Linux 均有)
    #[cfg(unix)]
    #[test]
    fn detect_kind_local_text_and_binary() {
        let dir = std::env::temp_dir().join(format!("rshell-test-kind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("a.txt");
        let bin = dir.join("b.bin");
        std::fs::write(&text, b"hello world\n").unwrap();
        std::fs::write(&bin, b"\x7fELF\x02\x01\x01\x00").unwrap();
        let text_mime = detect_kind_local(text.to_str().unwrap()).unwrap();
        assert!(text_mime.starts_with("text/"), "got: {text_mime}");
        let bin_mime = detect_kind_local(bin.to_str().unwrap()).unwrap();
        assert!(
            bin_mime.contains("executable") || bin_mime.contains("octet-stream"),
            "got: {bin_mime}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize() {
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("/root/"), "/root");
        assert_eq!(normalize_path("/root/../etc"), "/etc");
        assert_eq!(normalize_path("/a/./b//c"), "/a/b/c");
        assert_eq!(normalize_path("/../x"), "/x");
        assert_eq!(normalize_path("relative/path"), "/relative/path");
    }

    #[test]
    fn parent() {
        assert_eq!(parent_path("/root"), "/");
        assert_eq!(parent_path("/"), "/");
        assert_eq!(parent_path("/a/b/c"), "/a/b");
    }

    #[test]
    fn join() {
        assert_eq!(join_path("/", "etc"), "/etc");
        assert_eq!(join_path("/root", "a.txt"), "/root/a.txt");
        assert_eq!(join_path("/root/", "a.txt"), "/root/a.txt");
    }

    #[test]
    fn mode_fmt() {
        assert_eq!(format_mode(0o755, true), "drwxr-xr-x");
        assert_eq!(format_mode(0o644, false), "-rw-r--r--");
        assert_eq!(format_mode(0o600, false), "-rw-------");
    }

    #[test]
    fn civil() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20206), (2025, 4, 28));
    }

    #[test]
    fn local_worker_ops() {
        let dir = std::env::temp_dir().join(format!("rshell_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_s = dir.to_string_lossy().into_owned();

        // mkdir
        let sub = join_path(&dir_s, "sub");
        let ev = handle_local_cmd(SftpCmd::Mkdir(sub.clone()), &dummy_tx());
        assert!(matches!(
            ev,
            SftpEvent::OpDone {
                op: "mkdir",
                result: Ok(()),
                ..
            }
        ));

        // 写一个文件
        let f = join_path(&sub, "a.txt");
        std::fs::write(&f, b"hello rshell").unwrap();

        // list
        let ev = handle_local_cmd(SftpCmd::List(sub.clone()), &dummy_tx());
        match ev {
            SftpEvent::Listed {
                result: Ok(entries),
                ..
            } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].name, "a.txt");
                assert!(!entries[0].is_dir);
                assert_eq!(entries[0].size, 12);
                assert!(entries[0].mtime > 0);
            }
            _ => panic!("list failed: {:?}", std::mem::discriminant(&ev)),
        }

        // rename
        let f2 = join_path(&sub, "b.txt");
        let ev = handle_local_cmd(
            SftpCmd::Rename {
                from: f.clone(),
                to: f2.clone(),
            },
            &dummy_tx(),
        );
        assert!(matches!(
            ev,
            SftpEvent::OpDone {
                op: "rename",
                result: Ok(()),
                ..
            }
        ));
        assert!(!std::path::Path::new(&f).exists());
        assert!(std::path::Path::new(&f2).exists());

        // download(本地 = 拷贝)
        let dst = join_path(&dir_s, "copy.txt");
        let ev = handle_local_cmd(
            SftpCmd::Download {
                id: 1,
                remote: f2.clone(),
                local: dst.clone(),
            },
            &dummy_tx(),
        );
        assert!(matches!(
            ev,
            SftpEvent::OpDone {
                op: "download",
                result: Ok(()),
                ..
            }
        ));
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello rshell");

        // remove
        let ev = handle_local_cmd(
            SftpCmd::Remove {
                path: f2.clone(),
                is_dir: false,
            },
            &dummy_tx(),
        );
        assert!(matches!(
            ev,
            SftpEvent::OpDone {
                op: "remove",
                result: Ok(()),
                ..
            }
        ));
        assert!(!std::path::Path::new(&f2).exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 递归删除:非空目录连同内容一起删掉
    #[cfg(unix)]
    #[test]
    fn local_recursive_remove() {
        let dir = std::env::temp_dir().join(format!("rshell_rr_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/f.txt"), b"x").unwrap();
        std::fs::write(dir.join("a/b/c.txt"), b"y").unwrap();
        let target = dir.join("a").to_string_lossy().into_owned();

        let ev = handle_local_cmd(SftpCmd::RemoveRecursive { path: target }, &dummy_tx());
        assert!(matches!(
            ev,
            SftpEvent::OpDone {
                op: "remove",
                result: Ok(()),
                ..
            }
        ));
        assert!(!dir.join("a").exists(), "递归删除后目录应不存在");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 设置权限:改完可读回验证
    #[cfg(unix)]
    #[test]
    fn local_set_permissions() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("rshell_perm_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("p.txt");
        std::fs::write(&f, b"p").unwrap();
        let f_s = f.to_string_lossy().into_owned();

        let ev = handle_local_cmd(
            SftpCmd::SetPermissions {
                path: f_s.clone(),
                mode: 0o600,
            },
            &dummy_tx(),
        );
        assert!(matches!(
            ev,
            SftpEvent::OpDone {
                op: "chmod",
                result: Ok(()),
                ..
            }
        ));
        let mode = std::fs::metadata(&f).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600, "权限应改为 0600");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 符号链接指向目录时应按目录展示(Linux 的 /bin、macOS 的 /etc 都是链接)
    #[cfg(unix)]
    #[test]
    fn symlinked_dir_is_dir() {
        let dir = std::env::temp_dir().join(format!("rshell_lnk_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let dir_s = dir.to_string_lossy().into_owned();

        let ev = handle_local_cmd(SftpCmd::List(dir_s), &dummy_tx());
        let SftpEvent::Listed {
            result: Ok(entries),
            ..
        } = ev
        else {
            panic!("list failed");
        };
        let link = entries.iter().find(|e| e.name == "link").unwrap();
        assert!(link.is_dir, "symlink to dir should be treated as dir");
        assert!(link.is_symlink);
        let real = entries.iter().find(|e| e.name == "real").unwrap();
        assert!(real.is_dir && !real.is_symlink);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn large_copy_resumes_matching_partial_and_commits() {
        let dir = test_dir("resume");
        let source = dir.join("source.bin");
        let target = dir.join("target.bin");
        let data: Vec<u8> = (0..(1024 * 1024 + 73))
            .map(|index| (index % 251) as u8)
            .collect();
        std::fs::write(&source, &data).unwrap();
        let partial = local_partial_path(&target);
        std::fs::write(&partial, &data[..300_000]).unwrap();
        let tag = local_source_tag(&std::fs::metadata(&source).unwrap());
        std::fs::write(local_partial_meta_path(&partial), tag).unwrap();

        let event = copy_with_progress(
            1,
            &source.to_string_lossy(),
            &target.to_string_lossy(),
            "upload",
            &dummy_tx(),
            &egui::Context::default(),
            &TransferControl::default(),
        );
        assert!(matches!(event, SftpEvent::OpDone { result: Ok(()), .. }));
        assert_eq!(std::fs::read(&target).unwrap(), data);
        assert!(!partial.exists());
        assert!(!local_partial_meta_path(&partial).exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn canceled_copy_preserves_existing_target() {
        let dir = test_dir("cancel");
        let source = dir.join("source.bin");
        let target = dir.join("target.bin");
        std::fs::write(&source, vec![7u8; 1024 * 1024]).unwrap();
        std::fs::write(&target, b"existing").unwrap();
        let control = TransferControl::default();
        control.cancel();

        let event = copy_with_progress(
            2,
            &source.to_string_lossy(),
            &target.to_string_lossy(),
            "download",
            &dummy_tx(),
            &egui::Context::default(),
            &control,
        );
        assert!(matches!(event, SftpEvent::OpDone { result: Err(_), .. }));
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sftp_command_queue_is_bounded() {
        let (tx, _rx) = sftp_channel();
        for index in 0..128 {
            tx.try_send(SftpCmd::List(format!("/{index}"))).unwrap();
        }
        assert!(matches!(
            tx.try_send(SftpCmd::List("/full".into())),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));
    }

    fn test_dir(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rshell_transfer_{label}_{}_{}",
            std::process::id(),
            nonce
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn dummy_tx() -> std::sync::mpsc::Sender<crate::session::SessionEvent> {
        let (tx, _rx) = std::sync::mpsc::channel::<crate::session::SessionEvent>();
        tx
    }
}
