// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! 文件管理器状态:当前目录、列表、排序、目录树缓存、传输任务。
//! UI 在 ui/file_manager.rs,逻辑都在这里。

use std::collections::{HashMap, HashSet, VecDeque};
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
    /// 上传成功后需要刷新的远端目录（下载时为 None）。
    refresh_dir: Option<String>,
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
            refresh_dir: None,
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
    /// 弹窗刚打开：下一次绘制时聚焦名称框并全选，避免按键落到下层终端。
    pub mkdir_focus: bool,
    pub create_file_open: bool,
    pub create_file_name: String,
    pub create_file_focus: bool,
    pub rename_old: Option<String>,
    pub rename_name: String,
    pub rename_focus: bool,
    /// (路径, 是否目录, 是否递归)
    pub delete_target: Option<(String, bool, bool)>,
    /// 删除确认弹窗已经显示过至少一帧；避免打开弹窗的同一个回车立即确认删除。
    pub delete_armed: bool,
    /// 权限对话框目标
    pub perm_target: Option<String>,
    pub perm_armed: bool,
    /// 编辑中的权限位(低 9 位 rwxrwxrwx)
    pub perm_mode: u32,
    /// 原始完整 mode(含文件类型位,setstat 时保留类型位只改权限)
    pub perm_original: u32,
    /// 原始是否目录(显示 "d"/"-" 用)
    pub perm_is_dir: bool,
}

/// 目录树可见行：(路径, 深度, 是否展开, 是否加载中, 是否符号链接目录)
pub(crate) type TreeRow = (String, usize, bool, bool, bool);

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
    /// 导航后等待目录树把当前目录滚动到可视区域；节点尚未加载时保持请求。
    pub tree_scroll_to_cwd: bool,
    tree_rows_cache: Arc<Vec<TreeRow>>,
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
    pub preview_inbox: VecDeque<(u64, String, Result<Vec<u8>, String>)>,
    /// DetectKind 结果收件箱(app 层取走)
    pub kind_inbox: VecDeque<(u64, String, Result<String, String>)>,
    /// WriteFile(编辑器保存)结果收件箱(app 层取走)
    pub write_inbox: VecDeque<(u64, crate::session::files::WriteResult)>,
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
            tree_scroll_to_cwd: true,
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
            preview_inbox: VecDeque::new(),
            kind_inbox: VecDeque::new(),
            write_inbox: VecDeque::new(),
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
        self.set_current_directory(&norm);
        self.reveal_in_tree(&norm);
    }

    /// 从左侧目录树单击选择目录。更新右侧文件列表，但只展开祖先链，
    /// 不改变当前节点自己的展开状态；该节点由双击或箭头控制展开。
    pub fn select_tree_path(&mut self, path: &str) {
        let norm = normalize_path(path);
        self.set_current_directory(&norm);
        // 鼠标刚点击的行已经可见，不要在两次点击之间自动滚动，
        // 否则节点可能移出指针位置，导致双击无法成立。
        self.tree_scroll_to_cwd = false;
        if norm != "/" {
            self.reveal_in_tree(&parent_path(&norm));
        }
    }

    fn set_current_directory(&mut self, norm: &str) {
        if norm != self.cwd {
            if !self.history.contains(&self.cwd) {
                self.history.push(self.cwd.clone());
                if self.history.len() > 100 {
                    self.history.remove(0);
                }
            }
            self.cwd = norm.to_string();
        }
        self.path_edit = norm.to_string();
        self.selected = None;
        self.tree_scroll_to_cwd = true;
        // 允许重复请求(刷新当前目录数据)
        self.tree_loading.remove(norm);
        self.request_list(norm);
    }

    /// 在目录树中展开 path 及其祖先链,缺失的目录数据自动请求
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
        let cwd = self.cwd.clone();
        self.refresh_directory(&cwd);
    }

    /// 刷新指定目录。上传的目标目录可能已经不是当前 cwd，仍需更新其列表/
    /// 目录树缓存，而不是去刷新用户当前所在的目录。
    fn refresh_directory(&mut self, path: &str) {
        // 不清理 tree_children:旧数据留到新数据到达(Listed 时整体替换),
        // 否则目录树先塌陷再展开,下面的节点视觉上上下跳动
        let dir = normalize_path(path);
        self.tree_loading.remove(&dir);
        self.request_list(&dir);
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
                if matches!(op, "download" | "upload" | "download_dir" | "upload_dir") {
                    let mut refresh_dir = None;
                    if let Some(t) =
                        id.and_then(|id| self.transfers.iter_mut().find(|t| t.id == id))
                    {
                        t.finished = true;
                        t.set_paused(false);
                        t.finished_at = Some(std::time::Instant::now());
                        if let Err(e) = &result {
                            t.error = Some(e.clone());
                        } else {
                            refresh_dir = t.refresh_dir.clone();
                        }
                    }
                    if let Err(e) = &result {
                        self.error = Some(format!("{op} 失败:{e}"));
                    }
                    // 上传完成后目录里多了文件/目录，必须刷新目标目录，
                    // 否则用户得手动点“刷新”才能看到刚上传的内容。
                    if let Some(dir) = refresh_dir {
                        self.refresh_directory(&dir);
                    }
                } else {
                    match result {
                        Ok(()) => self.refresh(),
                        Err(e) => self.error = Some(format!("{op} 失败:{e}")),
                    }
                }
            }
            SftpEvent::FileWritten { id, result } => {
                // 允许同一帧完成多个编辑器保存，不能用单个 Option 覆盖先到的事件。
                self.write_inbox.push_back((id, result));
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
                self.preview_inbox.push_back((id, path, result));
            }
            SftpEvent::KindDetected { id, path, result } => {
                self.kind_inbox.push_back((id, path, result));
            }
        }
    }

    pub fn visible_tree_rows(&mut self) -> Arc<Vec<TreeRow>> {
        if self.tree_rows_dirty {
            let mut rows = Vec::new();
            let mut stack = vec![("/".to_string(), 0usize, false)];
            while let Some((path, depth, is_symlink)) = stack.pop() {
                let expanded = self.tree_expanded.contains(&path);
                let loading = self.tree_loading.contains(&path);
                rows.push((path.clone(), depth, expanded, loading, is_symlink));
                if expanded
                    && depth < 16
                    && let Some(children) = self.tree_children.get(&path)
                {
                    for child in children.iter().rev() {
                        stack.push((child.full_path(&path), depth + 1, child.is_symlink));
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
        let mut transfer = Transfer::new(id, "upload_dir", local_path, 0, 0);
        transfer.refresh_dir = Some(self.cwd.clone());
        self.transfers.push(transfer);
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
        let mut transfer = Transfer::new(id, "upload", local_path, 0, total);
        transfer.refresh_dir = Some(self.cwd.clone());
        self.transfers.push(transfer);
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

    fn directory(name: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir: true,
            size: 0,
            mtime: 0,
            permissions: 0o40755,
            uid: 0,
            gid: 0,
            owner: String::new(),
            group: String::new(),
            is_symlink: false,
            is_broken_symlink: false,
        }
    }

    #[test]
    fn local_mkdir_refreshes_listing_without_manual_refresh() {
        use crate::session::SessionEvent;
        use std::time::{Duration, Instant};

        let root =
            std::env::temp_dir().join(format!("rshell_fm_mkdir_refresh_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let ctx = egui::Context::default();
        let (sftp_tx, sftp_rx) = tokio::sync::mpsc::channel(64);
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        crate::session::files::spawn_local_worker(sftp_rx, event_tx, ctx);

        let mut manager = FileManager::new(Some(sftp_tx));
        manager.cwd = root.to_string_lossy().into_owned();
        manager.path_edit = manager.cwd.clone();
        manager.started = true;
        let cwd = manager.cwd.clone();
        manager.request_list(&cwd);

        let pump = |manager: &mut FileManager| {
            while let Ok(event) = event_rx.try_recv() {
                if let SessionEvent::Sftp(event) = event {
                    manager.handle_event(event);
                }
            }
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while manager.loading && Instant::now() < deadline {
            pump(&mut manager);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!manager.loading, "初始目录列表应完成");

        let created = root.join("created-dir");
        manager
            .send(SftpCmd::Mkdir(created.to_string_lossy().into_owned()))
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut found = false;
        while Instant::now() < deadline {
            pump(&mut manager);
            if manager
                .entries
                .iter()
                .any(|entry| entry.name == "created-dir")
            {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(found, "新建目录后列表应自动刷新，无需手动点刷新");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn successful_upload_refreshes_the_upload_target_directory() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let mut manager = FileManager::new(Some(tx));
        manager.cwd = "/srv/app".into();
        manager.start_upload("/tmp/new-file.bin".into());
        let id = loop {
            match rx.try_recv().expect("upload command") {
                SftpCmd::Upload { id, .. } => break id,
                SftpCmd::List(_) => panic!("上传前不应刷新"),
                _ => {}
            }
        };

        // 上传本身不应该提前刷新
        while let Ok(cmd) = rx.try_recv() {
            assert!(!matches!(cmd, SftpCmd::List(_)));
        }

        manager.handle_event(SftpEvent::OpDone {
            id: Some(id),
            op: "upload",
            result: Ok(()),
        });

        let mut listed = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let SftpCmd::List(path) = cmd {
                listed.push(path);
            }
        }
        assert_eq!(
            listed,
            vec!["/srv/app".to_string()],
            "上传完成后应自动刷新目标目录"
        );
    }

    #[test]
    fn successful_upload_dir_refreshes_the_parent_directory() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let mut manager = FileManager::new(Some(tx));
        manager.cwd = "/srv/app".into();
        manager.start_upload_dir("/tmp/new-dir".into());

        let id = loop {
            match rx.try_recv().expect("upload_dir command") {
                SftpCmd::UploadDir { id, .. } => break id,
                SftpCmd::List(_) => panic!("upload_dir 前不应刷新"),
                _ => {}
            }
        };
        manager.handle_event(SftpEvent::OpDone {
            id: Some(id),
            op: "upload_dir",
            result: Ok(()),
        });

        let mut listed = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let SftpCmd::List(path) = cmd {
                listed.push(path);
            }
        }
        assert_eq!(listed, vec!["/srv/app".to_string()]);
    }

    #[test]
    fn successful_mkdir_refreshes_the_current_directory() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let mut manager = FileManager::new(Some(tx));
        manager.cwd = "/srv/app".into();
        manager.handle_event(SftpEvent::OpDone {
            id: None,
            op: "mkdir",
            result: Ok(()),
        });
        let mut listed = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let SftpCmd::List(path) = cmd {
                listed.push(path);
            }
        }
        assert_eq!(listed, vec!["/srv/app".to_string()]);
    }

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

    #[test]
    fn editor_write_results_are_queued_without_overwriting() {
        use crate::session::files::{WriteFailure, WriteFailureKind};

        let mut manager = FileManager::new(None);
        manager.handle_event(SftpEvent::FileWritten {
            id: 11,
            result: Ok(()),
        });
        manager.handle_event(SftpEvent::FileWritten {
            id: 12,
            result: Err(WriteFailure::new(WriteFailureKind::Conflict, "changed")),
        });

        assert_eq!(manager.write_inbox.len(), 2);
        assert_eq!(manager.write_inbox.pop_front().unwrap().0, 11);
        let (id, result) = manager.write_inbox.pop_front().unwrap();
        assert_eq!(id, 12);
        assert_eq!(result.unwrap_err().kind, WriteFailureKind::Conflict);
    }

    #[test]
    fn concurrent_preview_results_are_queued_without_overwriting() {
        let mut manager = FileManager::new(None);
        manager.handle_event(SftpEvent::FileContent {
            id: 21,
            path: "/tmp/first".into(),
            result: Ok(b"first".to_vec()),
        });
        manager.handle_event(SftpEvent::FileContent {
            id: 22,
            path: "/tmp/second".into(),
            result: Ok(b"second".to_vec()),
        });

        assert_eq!(manager.preview_inbox.len(), 2);
        assert_eq!(manager.preview_inbox.pop_front().unwrap().0, 21);
        assert_eq!(manager.preview_inbox.pop_front().unwrap().0, 22);
    }

    #[test]
    fn navigation_expands_and_reveals_the_current_directory() {
        let mut manager = FileManager::new(None);
        manager.cwd = "/".into();
        manager.tree_expanded.clear();
        manager.tree_scroll_to_cwd = false;
        manager
            .tree_children
            .insert("/".into(), vec![directory("srv")]);
        manager
            .tree_children
            .insert("/srv".into(), vec![directory("app")]);
        manager
            .tree_children
            .insert("/srv/app".into(), vec![directory("logs")]);
        manager
            .tree_children
            .insert("/srv/app/logs".into(), vec![directory("archive")]);

        manager.navigate("/srv/app/logs");

        assert_eq!(manager.cwd, "/srv/app/logs");
        assert!(manager.tree_scroll_to_cwd);
        for path in ["/", "/srv", "/srv/app", "/srv/app/logs"] {
            assert!(
                manager.tree_expanded.contains(path),
                "navigation did not expand {path}"
            );
        }
        let rows = manager.visible_tree_rows();
        assert!(
            rows.iter()
                .any(|(path, _, expanded, _, _)| { path == "/srv/app/logs" && *expanded })
        );
        assert!(
            rows.iter()
                .any(|(path, _, _, _, _)| path == "/srv/app/logs/archive"),
            "expanded current directory should expose its loaded children"
        );
    }

    #[test]
    fn tree_rows_mark_symlinked_directories() {
        let mut manager = FileManager::new(None);
        manager.cwd = "/".into();
        manager.tree_expanded.insert("/".into());
        let mut link = directory("link-dir");
        link.is_symlink = true;
        manager.tree_children.insert("/".into(), vec![link]);

        let rows = manager.visible_tree_rows();
        assert!(
            rows.iter()
                .any(|(path, _, _, _, is_link)| path == "/link-dir" && *is_link),
            "目录树应把符号链接目录标记出来"
        );
    }

    #[test]
    fn tree_selection_keeps_the_selected_node_collapsed() {
        let mut manager = FileManager::new(None);
        manager.cwd = "/".into();
        manager.tree_expanded.clear();
        manager
            .tree_children
            .insert("/".into(), vec![directory("srv")]);
        manager
            .tree_children
            .insert("/srv".into(), vec![directory("app")]);

        manager.select_tree_path("/srv");

        assert_eq!(manager.cwd, "/srv");
        assert!(manager.tree_expanded.contains("/"));
        assert!(!manager.tree_expanded.contains("/srv"));
        assert!(
            manager
                .visible_tree_rows()
                .iter()
                .all(|(path, _, _, _, _)| path != "/srv/app")
        );
    }
}
