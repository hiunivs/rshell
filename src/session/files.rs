//! 文件管理共享模型:FileEntry、SFTP 命令/事件、路径与时间工具。
//! 远程会话经 russh-sftp 实现,本机会话经 std::fs 实现,UI 统一消费。

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
    /// 设置 unix 权限位(含文件类型位,直接传给 setstat)
    SetPermissions {
        path: String,
        mode: u32,
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
}

pub type SftpTx = tokio::sync::mpsc::UnboundedSender<SftpCmd>;
pub type SftpRx = tokio::sync::mpsc::UnboundedReceiver<SftpCmd>;

pub fn sftp_channel() -> (SftpTx, SftpRx) {
    tokio::sync::mpsc::unbounded_channel()
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
    std::thread::spawn(move || {
        while let Some(cmd) = rx.blocking_recv() {
            let ev = handle_local_cmd_with_ctx(cmd, &event_tx, &ctx);
            let _ = event_tx.send(super::SessionEvent::Sftp(ev));
            ctx.request_repaint();
        }
    });
}

fn handle_local_cmd_with_ctx(
    cmd: SftpCmd,
    event_tx: &std::sync::mpsc::Sender<super::SessionEvent>,
    ctx: &egui::Context,
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
            copy_with_progress(id, &remote, &local, "download", event_tx, ctx)
        }
        SftpCmd::UploadDir { id, local, remote } => {
            let result = copy_dir_with_progress(id, &local, &remote, "upload_dir", event_tx, ctx);
            SftpEvent::OpDone {
                id: Some(id),
                op: "upload_dir",
                result,
            }
        }
        SftpCmd::DownloadDir { id, remote, local } => {
            let result = copy_dir_with_progress(id, &remote, &local, "download_dir", event_tx, ctx);
            SftpEvent::OpDone {
                id: Some(id),
                op: "download_dir",
                result,
            }
        }
        SftpCmd::Upload { id, local, remote } => {
            copy_with_progress(id, &local, &remote, "upload", event_tx, ctx)
        }
        SftpCmd::SetPermissions { path, mode } => SftpEvent::OpDone {
            id: None,
            op: "chmod",
            result: set_local_perms(&path, mode),
        },
    }
}

#[cfg(test)]
fn handle_local_cmd(
    cmd: SftpCmd,
    event_tx: &std::sync::mpsc::Sender<super::SessionEvent>,
) -> SftpEvent {
    handle_local_cmd_with_ctx(cmd, event_tx, &egui::Context::default())
}

/// 遍历本地目录,收集(相对路径, 文件大小)
pub fn walk_files(base: &str, rel: String, out: &mut Vec<(String, u64)>) -> std::io::Result<()> {
    let full = if rel.is_empty() {
        std::path::PathBuf::from(base)
    } else {
        std::path::Path::new(base).join(&rel)
    };
    for entry in std::fs::read_dir(&full)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if meta.is_dir() {
            walk_files(base, child, out)?;
        } else {
            out.push((child, meta.len()));
        }
    }
    Ok(())
}

/// 递归拷贝目录:先遍历收集文件列表+总大小,再逐文件拷贝并报告进度
fn copy_dir_with_progress(
    id: u64,
    src_base: &str,
    dst_base: &str,
    op: &'static str,
    event_tx: &std::sync::mpsc::Sender<super::SessionEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    use std::io::{Read, Write};
    let mut files = Vec::new();
    walk_files(src_base, String::new(), &mut files)
        .map_err(|e| format!("遍历目录失败 {src_base}: {e}"))?;
    let total: u64 = files.iter().map(|(_, s)| *s).sum();
    let mut done = 0u64;
    for (rel, _size) in files {
        let src = std::path::Path::new(src_base).join(&rel);
        let dst = std::path::Path::new(dst_base).join(&rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {}: {e}", parent.display()))?;
        }
        let mut src_f =
            std::fs::File::open(&src).map_err(|e| format!("无法读取 {}: {e}", src.display()))?;
        let mut dst_f =
            std::fs::File::create(&dst).map_err(|e| format!("无法创建 {}: {e}", dst.display()))?;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = src_f
                .read(&mut buf)
                .map_err(|e| format!("读取 {} 失败: {e}", src.display()))?;
            if n == 0 {
                break;
            }
            dst_f
                .write_all(&buf[..n])
                .map_err(|e| format!("写入 {} 失败: {e}", dst.display()))?;
            done += n as u64;
            let _ = event_tx.send(super::SessionEvent::Sftp(SftpEvent::Progress {
                id,
                op,
                path: src_base.to_string(),
                done,
                total,
            }));
            ctx.request_repaint();
        }
    }
    Ok(())
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
) -> SftpEvent {
    use std::io::{Read, Write};
    let result = (|| -> Result<(), String> {
        let mut src = std::fs::File::open(from).map_err(|e| format!("无法读取 {from}: {e}"))?;
        let total = src.metadata().map_err(|e| e.to_string())?.len();
        let mut dst = std::fs::File::create(to).map_err(|e| format!("无法创建 {to}: {e}"))?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            let n = src
                .read(&mut buf)
                .map_err(|e| format!("读取 {from} 失败: {e}"))?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n])
                .map_err(|e| format!("写入 {to} 失败: {e}"))?;
            done += n as u64;
            let _ = event_tx.send(super::SessionEvent::Sftp(SftpEvent::Progress {
                id,
                op,
                path: from.to_string(),
                done,
                total,
            }));
            ctx.request_repaint();
        }
        Ok(())
    })();
    SftpEvent::OpDone {
        id: Some(id),
        op,
        result,
    }
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

    fn dummy_tx() -> std::sync::mpsc::Sender<crate::session::SessionEvent> {
        let (tx, _rx) = std::sync::mpsc::channel::<crate::session::SessionEvent>();
        tx
    }
}
