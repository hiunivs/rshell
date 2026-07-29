//! 文件管理器状态:当前目录、列表、排序、目录树缓存、传输任务。
//! UI 在 ui/file_manager.rs,逻辑都在这里。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::session::files::{
    FileEntry, SftpCmd, SftpEvent, SftpTx, join_path, normalize_path, parent_path,
};

/// 本机用户主目录(仅 unix;其他平台回退 "/")
fn home_dir() -> Option<String> {
    #[cfg(unix)]
    {
        std::env::var("HOME").ok().filter(|h| !h.is_empty())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortCol {
    Name,
    Size,
    Time,
}

/// 传输窗的显示位置
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TransferPos {
    Bottom,
    Right,
}

pub struct Transfer {
    pub id: u64,
    pub op: &'static str,
    pub path: String,
    pub done: u64,
    pub total: u64,
    pub finished: bool,
    pub paused: bool,
    pub finished_at: Option<std::time::Instant>,
    pub error: Option<String>,
    speed_bps: f64,
    rate_sample_done: u64,
    rate_sample_at: std::time::Instant,
    received_progress: bool,
}

impl Transfer {
    pub(crate) fn new(id: u64, op: &'static str, path: String, done: u64, total: u64) -> Self {
        Self {
            id,
            op,
            path,
            done,
            total,
            finished: false,
            paused: false,
            finished_at: None,
            error: None,
            speed_bps: 0.0,
            rate_sample_done: done,
            rate_sample_at: std::time::Instant::now(),
            received_progress: false,
        }
    }

    fn update_progress(&mut self, done: u64, total: u64) {
        self.update_progress_at(done, total, std::time::Instant::now());
    }

    fn update_progress_at(&mut self, done: u64, total: u64, now: std::time::Instant) {
        self.done = done;
        self.total = total;
        if !self.received_progress || done < self.rate_sample_done {
            self.speed_bps = 0.0;
            self.rate_sample_done = done;
            self.rate_sample_at = now;
            self.received_progress = true;
            return;
        }

        let elapsed = now.saturating_duration_since(self.rate_sample_at);
        if elapsed < std::time::Duration::from_millis(50) {
            return;
        }
        let instant_speed =
            done.saturating_sub(self.rate_sample_done) as f64 / elapsed.as_secs_f64();
        self.speed_bps = if self.speed_bps <= 0.0 {
            instant_speed
        } else {
            // 指数平滑，保留趋势同时避免 100ms 采样值剧烈跳动。
            self.speed_bps * 0.7 + instant_speed * 0.3
        };
        self.rate_sample_done = done;
        self.rate_sample_at = now;
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.speed_bps = 0.0;
        self.rate_sample_done = self.done;
        self.rate_sample_at = std::time::Instant::now();
        self.received_progress = false;
    }

    pub fn speed_bps(&self) -> f64 {
        if self.paused
            || self.finished
            || std::time::Instant::now().saturating_duration_since(self.rate_sample_at)
                > std::time::Duration::from_secs(2)
        {
            0.0
        } else {
            self.speed_bps
        }
    }
}

/// 弹窗状态
#[derive(Default)]
pub struct FmDialogs {
    pub mkdir_open: bool,
    pub mkdir_name: String,
    pub rename_old: Option<String>,
    pub rename_name: String,
    /// (路径, 是否目录, 是否递归)
    pub delete_target: Option<(String, bool, bool)>,
    /// 权限对话框目标
    pub perm_target: Option<String>,
    /// 编辑中的权限位(低 9 位 rwxrwxrwx)
    pub perm_mode: u32,
    /// 原始完整 mode(含文件类型位,setstat 时保留类型位只改权限)
    pub perm_original: u32,
    /// 原始是否目录(显示 "d"/"-" 用)
    pub perm_is_dir: bool,
}

pub struct FileManager {
    pub cwd: String,
    pub path_edit: String,
    pub entries: Vec<FileEntry>,
    pub loading: bool,
    pub error: Option<String>,
    pub sort_col: SortCol,
    pub sort_asc: bool,
    pub selected: Option<usize>,
    /// 最后一次明确点击的是文件表行；用于限定复制快捷键作用域。
    pub table_keyboard_active: bool,
    /// 目录树栏宽(可拖拽调整)
    pub tree_width: f32,
    // 目录树
    pub tree_expanded: HashSet<String>,
    pub tree_children: HashMap<String, Vec<FileEntry>>,
    pub tree_loading: HashSet<String>,
    tree_rows_cache: Arc<Vec<(String, usize, bool, bool)>>,
    tree_rows_dirty: bool,
    // 传输
    pub transfers: Vec<Transfer>,
    // 历史路径
    pub history: Vec<String>,
    pub sftp_tx: Option<SftpTx>,
    started: bool,
    /// 是否已应用过 HomeDir(避免用户浏览中被拉回主目录)
    applied_home: bool,
    pub dialogs: FmDialogs,
    /// 传输窗是否展开(点击工具栏按钮切换)
    pub show_transfers_pane: bool,
    /// 传输窗位置
    pub transfer_pos: TransferPos,
    next_transfer_id: u64,
    // ---- 在线预览/编辑 ----
    /// UI 请求预览/编辑的文件(双击产生,app 层消费)
    pub request_preview: Option<FileEntry>,
    /// UI 请求用本地程序打开的文件(右键菜单产生,app 层消费)
    pub request_external_open: Option<FileEntry>,
    /// ReadFile 结果收件箱(app 层取走)
    pub preview_inbox: Option<(u64, String, Result<Vec<u8>, String>)>,
    /// DetectKind 结果收件箱(app 层取走)
    pub kind_inbox: Option<(u64, String, Result<String, String>)>,
    /// WriteFile(编辑器保存)结果收件箱(app 层取走)
    pub write_inbox: Option<(u64, Result<(), String>)>,
}

/// 通用操作 ID(预览/编辑/外部打开)——与传输 ID 共用同一计数器,
/// 避免不同通道的事件 ID 冲突
pub type OpId = u64;

impl FileManager {
    /// cwd 为 None 时用系统主目录(不可用时回退 "/")
    pub fn new(sftp_tx: Option<SftpTx>) -> Self {
        let cwd = home_dir().unwrap_or_else(|| "/".into());
        Self {
            path_edit: cwd.clone(),
            cwd,
            entries: Vec::new(),
            loading: false,
            error: None,
            sort_col: SortCol::Name,
            sort_asc: true,
            selected: None,
            table_keyboard_active: false,
            tree_width: 180.0,
            tree_expanded: HashSet::from(["/".to_string()]),
            tree_children: HashMap::new(),
            tree_loading: HashSet::new(),
            tree_rows_cache: Arc::new(Vec::new()),
            tree_rows_dirty: true,
            transfers: Vec::new(),
            history: Vec::new(),
            sftp_tx,
            started: false,
            applied_home: false,
            dialogs: FmDialogs::default(),
            show_transfers_pane: true,
            transfer_pos: TransferPos::Bottom,
            next_transfer_id: 1,
            request_preview: None,
            request_external_open: None,
            preview_inbox: None,
            kind_inbox: None,
            write_inbox: None,
        }
    }

    /// SSH 会话:初始为远程根目录,家目录由 HomeDir 事件解析后自动进入
    pub fn new_ssh(sftp_tx: Option<SftpTx>) -> Self {
        let mut fm = Self::new(sftp_tx);
        fm.cwd = "/".into();
        fm.path_edit = "/".into();
        fm
    }

    /// 每帧调用:通道就绪后自动请求当前目录
    pub fn tick(&mut self) {
        self.prune_transfers();
        if !self.started && self.sftp_tx.is_some() {
            self.started = true;
            let cwd = self.cwd.clone();
            self.request_list(&cwd);
            self.reveal_in_tree(&cwd);
        }
    }

    fn prune_transfers(&mut self) {
        const KEEP_FOR: std::time::Duration = std::time::Duration::from_secs(60);
        const MAX_COMPLETED: usize = 50;
        let now = std::time::Instant::now();
        self.transfers.retain(|transfer| {
            !transfer.finished
                || transfer
                    .finished_at
                    .is_none_or(|at| now.duration_since(at) < KEEP_FOR)
        });
        let completed = self.transfers.iter().filter(|t| t.finished).count();
        if completed > MAX_COMPLETED {
            let mut remove = completed - MAX_COMPLETED;
            self.transfers.retain(|t| {
                if t.finished && remove > 0 {
                    remove -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    pub fn send(&self, cmd: SftpCmd) -> Result<(), String> {
        let tx = self
            .sftp_tx
            .as_ref()
            .ok_or_else(|| "文件服务尚未就绪".to_string())?;
        let is_control = matches!(
            &cmd,
            SftpCmd::CancelTransfer { .. }
                | SftpCmd::PauseTransfer { .. }
                | SftpCmd::ResumeTransfer { .. }
        );
        // 为暂停/取消预留槽位，普通任务不能占满整个队列。
        if !is_control && tx.capacity() <= 8 {
            return Err("文件任务队列已满，请稍后重试".into());
        }
        tx.try_send(cmd).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                "文件任务队列已满，请稍后重试".to_string()
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => "文件服务连接已关闭".to_string(),
        })
    }

    pub fn request_list(&mut self, path: &str) {
        if self.sftp_tx.is_none() {
            return;
        }
        // 同一路径已在加载中则跳过,避免重复请求堆积
        if self.tree_loading.contains(path) {
            return;
        }
        if normalize_path(path) == self.cwd {
            self.loading = true;
        }
        self.tree_loading.insert(path.to_string());
        self.tree_rows_dirty = true;
        if let Err(error) = self.send(SftpCmd::List(path.to_string())) {
            self.tree_loading.remove(path);
            self.tree_rows_dirty = true;
            self.loading = false;
            self.error = Some(error);
        }
    }

    pub fn navigate(&mut self, path: &str) {
        let norm = normalize_path(path);
        if norm != self.cwd {
            if !self.history.contains(&self.cwd) {
                self.history.push(self.cwd.clone());
                if self.history.len() > 100 {
                    self.history.remove(0);
                }
            }
            self.cwd = norm.clone();
        }
        self.path_edit = norm.clone();
        self.selected = None;
        // 允许重复请求(刷新当前目录数据)
        self.tree_loading.remove(&norm);
        self.request_list(&norm);
        self.reveal_in_tree(&norm);
    }

    /// 在目录树中展开 path 的祖先链,缺失的目录数据自动请求
    pub fn reveal_in_tree(&mut self, path: &str) {
        let norm = normalize_path(path);
        let mut cur = "/".to_string();
        self.tree_expanded.insert(cur.clone());
        self.tree_rows_dirty = true;
        if !self.tree_children.contains_key(&cur) {
            self.request_list(&cur);
        }
        for seg in norm.split('/').filter(|s| !s.is_empty()) {
            let next = if cur == "/" {
                format!("/{seg}")
            } else {
                format!("{cur}/{seg}")
            };
            if next == norm {
                break; // 当前目录本身不必展开,高亮即可
            }
            self.tree_expanded.insert(next.clone());
            self.tree_rows_dirty = true;
            if !self.tree_children.contains_key(&next) {
                self.request_list(&next);
            }
            cur = next;
        }
    }

    /// 展开树节点(点击目录名时:展开 + 进入)
    pub fn expand_tree(&mut self, path: &str) {
        self.tree_expanded.insert(path.to_string());
        self.tree_rows_dirty = true;
        if !self.tree_children.contains_key(path) {
            self.request_list(path);
        }
    }

    pub fn go_parent(&mut self) {
        let parent = parent_path(&self.cwd);
        self.navigate(&parent);
    }

    pub fn refresh(&mut self) {
        // 不清理 tree_children:旧数据留到新数据到达(Listed 时整体替换),
        // 否则目录树先塌陷再展开,下面的节点视觉上上下跳动
        let cwd = self.cwd.clone();
        self.tree_loading.remove(&cwd);
        self.request_list(&cwd);
    }

    pub fn toggle_tree(&mut self, path: &str) {
        if !self.tree_expanded.remove(path) {
            self.tree_expanded.insert(path.to_string());
            if !self.tree_children.contains_key(path) {
                self.request_list(path);
            }
        }
        self.tree_rows_dirty = true;
    }

    pub fn sort_by(&mut self, col: SortCol) {
        if self.sort_col == col {
            self.sort_asc = !self.sort_asc;
        } else {
            self.sort_col = col;
            self.sort_asc = true;
        }
        self.sort();
    }

    fn sort(&mut self) {
        let asc = self.sort_asc;
        let col = self.sort_col;
        self.entries.sort_by(|a, b| {
            // 目录始终在前
            let dirs = b.is_dir.cmp(&a.is_dir);
            if dirs != std::cmp::Ordering::Equal {
                return dirs;
            }
            let ord = match col {
                SortCol::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortCol::Size => a.size.cmp(&b.size),
                SortCol::Time => a.mtime.cmp(&b.mtime),
            };
            if asc { ord } else { ord.reverse() }
        });
    }

    pub fn handle_event(&mut self, ev: SftpEvent) {
        match ev {
            SftpEvent::Listed { path, result } => {
                self.tree_loading.remove(&path);
                self.tree_rows_dirty = true;
                match result {
                    Ok(mut entries) => {
                        let dirs: Vec<FileEntry> =
                            entries.iter().filter(|e| e.is_dir).cloned().collect();
                        self.tree_children.insert(path.clone(), dirs);
                        self.trim_tree_cache();
                        self.tree_rows_dirty = true;
                        if normalize_path(&path) == self.cwd {
                            self.entries = std::mem::take(&mut entries);
                            self.sort();
                            self.loading = false;
                            self.error = None;
                        }
                    }
                    Err(e) => {
                        if normalize_path(&path) == self.cwd {
                            self.loading = false;
                            self.error = Some(e);
                        }
                    }
                }
            }
            SftpEvent::SetupFailed(e) => {
                self.loading = false;
                self.error = Some(e);
            }
            SftpEvent::HomeDir(home) => {
                // 只在用户还没浏览过时应用一次,避免打断用户
                if !self.applied_home {
                    self.applied_home = true;
                    if self.history.is_empty() && self.cwd == "/" {
                        self.navigate(&home);
                    }
                }
            }
            SftpEvent::OpDone { id, op, result } => {
                if op == "write" {
                    // 编辑器保存完成:不触发目录刷新,交给 app 层的编辑器状态
                    self.write_inbox = id.map(|id| (id, result));
                } else if matches!(op, "download" | "upload" | "download_dir" | "upload_dir") {
                    if let Some(t) =
                        id.and_then(|id| self.transfers.iter_mut().find(|t| t.id == id))
                    {
                        t.finished = true;
                        t.set_paused(false);
                        t.finished_at = Some(std::time::Instant::now());
                        if let Err(e) = &result {
                            t.error = Some(e.clone());
                        }
                    }
                    if let Err(e) = result {
                        self.error = Some(format!("{op} 失败:{e}"));
                    }
                } else {
                    match result {
                        Ok(()) => self.refresh(),
                        Err(e) => self.error = Some(format!("{op} 失败:{e}")),
                    }
                }
            }
            SftpEvent::Progress {
                id,
                op,
                path,
                done,
                total,
            } => {
                if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                    t.update_progress(done, total);
                } else {
                    let mut transfer = Transfer::new(id, op, path, done, total);
                    transfer.received_progress = true;
                    self.transfers.push(transfer);
                }
            }
            SftpEvent::FileContent { id, path, result } => {
                self.preview_inbox = Some((id, path, result));
            }
            SftpEvent::KindDetected { id, path, result } => {
                self.kind_inbox = Some((id, path, result));
            }
        }
    }

    pub fn visible_tree_rows(&mut self) -> Arc<Vec<(String, usize, bool, bool)>> {
        if self.tree_rows_dirty {
            let mut rows = Vec::new();
            let mut stack = vec![("/".to_string(), 0usize)];
            while let Some((path, depth)) = stack.pop() {
                let expanded = self.tree_expanded.contains(&path);
                let loading = self.tree_loading.contains(&path);
                rows.push((path.clone(), depth, expanded, loading));
                if expanded
                    && depth < 16
                    && let Some(children) = self.tree_children.get(&path)
                {
                    for child in children.iter().rev() {
                        stack.push((child.full_path(&path), depth + 1));
                    }
                }
            }
            self.tree_rows_cache = Arc::new(rows);
            self.tree_rows_dirty = false;
        }
        Arc::clone(&self.tree_rows_cache)
    }

    fn trim_tree_cache(&mut self) {
        const MAX_CACHED_DIRS: usize = 512;
        if self.tree_children.len() <= MAX_CACHED_DIRS {
            return;
        }
        let mut removable: Vec<_> = self
            .tree_children
            .keys()
            .filter(|path| {
                let protected = path.as_str() == "/"
                    || **path == self.cwd
                    || self.cwd.starts_with(&format!("{path}/"));
                !protected && !self.tree_expanded.contains(*path)
            })
            .cloned()
            .collect();
        if removable.len() < self.tree_children.len() - MAX_CACHED_DIRS {
            let extra: Vec<_> = self
                .tree_children
                .keys()
                .filter(|path| {
                    let protected = path.as_str() == "/"
                        || **path == self.cwd
                        || self.cwd.starts_with(&format!("{path}/"));
                    !protected && !removable.contains(path)
                })
                .cloned()
                .collect();
            removable.extend(extra);
        }
        removable.truncate(self.tree_children.len() - MAX_CACHED_DIRS);
        for path in removable {
            self.tree_children.remove(&path);
            self.tree_expanded.remove(&path);
        }
    }

    /// 下载选中的文件到本地路径
    /// 下载单个文件;返回传输 ID(发送失败返回 None 并设置 error)
    pub fn start_download(&mut self, entry: &FileEntry, local_path: String) -> Option<u64> {
        let remote = entry.full_path(&self.cwd);
        let id = self.next_transfer_id();
        if let Err(error) = self.send(SftpCmd::Download {
            id,
            remote: remote.clone(),
            local: local_path,
        }) {
            self.error = Some(error);
            return None;
        }
        self.transfers
            .push(Transfer::new(id, "download", remote, 0, entry.size));
        Some(id)
    }

    /// 下载整个目录到本地
    pub fn start_download_dir(&mut self, entry: &FileEntry, local_dir: String) {
        let remote = entry.full_path(&self.cwd);
        let id = self.next_transfer_id();
        if let Err(error) = self.send(SftpCmd::DownloadDir {
            id,
            remote: remote.clone(),
            local: local_dir,
        }) {
            self.error = Some(error);
            return;
        }
        self.transfers
            .push(Transfer::new(id, "download_dir", remote, 0, 0));
    }

    /// 上传整个本地目录到当前远程目录
    pub fn start_upload_dir(&mut self, local_path: String) {
        let dir_name = local_path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("upload")
            .to_string();
        let remote = join_path(&self.cwd, &dir_name);
        let id = self.next_transfer_id();
        if let Err(error) = self.send(SftpCmd::UploadDir {
            id,
            local: local_path.clone(),
            remote,
        }) {
            self.error = Some(error);
            return;
        }
        self.transfers
            .push(Transfer::new(id, "upload_dir", local_path, 0, 0));
    }

    /// 上传本地文件到当前目录
    pub fn start_upload(&mut self, local_path: String) {
        let name = local_path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("upload.bin")
            .to_string();
        let remote = join_path(&self.cwd, &name);
        let total = std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
        let id = self.next_transfer_id();
        if let Err(error) = self.send(SftpCmd::Upload {
            id,
            local: local_path.clone(),
            remote,
        }) {
            self.error = Some(error);
            return;
        }
        self.transfers
            .push(Transfer::new(id, "upload", local_path, 0, total));
    }

    fn next_transfer_id(&mut self) -> u64 {
        let id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1).max(1);
        id
    }

    /// 预览/编辑/外部打开等通用操作的 ID(与传输 ID 同一计数器,全局唯一)
    pub fn next_op_id(&mut self) -> OpId {
        self.next_transfer_id()
    }

    pub fn cancel_transfer(&mut self, id: u64) {
        match self.send(SftpCmd::CancelTransfer { id }) {
            Ok(()) => {
                if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == id) {
                    transfer.set_paused(false);
                }
            }
            Err(error) => self.error = Some(error),
        }
    }

    pub fn toggle_pause_transfer(&mut self, id: u64) {
        let Some(index) = self.transfers.iter().position(|t| t.id == id) else {
            return;
        };
        let paused = self.transfers[index].paused;
        let command = if paused {
            SftpCmd::ResumeTransfer { id }
        } else {
            SftpCmd::PauseTransfer { id }
        };
        match self.send(command) {
            Ok(()) => self.transfers[index].set_paused(!paused),
            Err(error) => self.error = Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_matches_transfer_id_not_operation_order() {
        let mut manager = FileManager::new(None);
        manager.transfers = vec![
            Transfer::new(10, "download", "/first".into(), 0, 1),
            Transfer::new(11, "download", "/second".into(), 0, 1),
        ];

        manager.handle_event(SftpEvent::OpDone {
            id: Some(10),
            op: "download",
            result: Ok(()),
        });

        assert!(manager.transfers[0].finished);
        assert!(!manager.transfers[1].finished);
    }

    #[test]
    fn transfer_rate_is_smoothed_and_resets_after_pause() {
        let mut transfer = Transfer::new(1, "upload", "/large.bin".into(), 0, 10_000);
        let start = transfer.rate_sample_at;
        transfer.update_progress_at(0, 10_000, start);
        transfer.update_progress_at(1_000, 10_000, start + std::time::Duration::from_secs(1));
        assert!((transfer.speed_bps - 1_000.0).abs() < 0.01);

        transfer.update_progress_at(3_000, 10_000, start + std::time::Duration::from_secs(2));
        assert!((transfer.speed_bps - 1_300.0).abs() < 0.01);

        transfer.set_paused(true);
        assert_eq!(transfer.speed_bps(), 0.0);
        transfer.set_paused(false);
        assert_eq!(transfer.speed_bps(), 0.0);
    }

    #[test]
    fn failed_enqueue_does_not_create_transfer() {
        let mut manager = FileManager::new(None);
        manager.start_upload("/does/not/exist".into());
        assert!(manager.transfers.is_empty());
        assert_eq!(manager.error.as_deref(), Some("文件服务尚未就绪"));
    }
}
