//! 文件管理器状态:当前目录、列表、排序、目录树缓存、传输任务。
//! UI 在 ui/file_manager.rs,逻辑都在这里。

use std::collections::{HashMap, HashSet};

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
    pub error: Option<String>,
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
    /// 目录树栏宽(可拖拽调整)
    pub tree_width: f32,
    // 目录树
    pub tree_expanded: HashSet<String>,
    pub tree_children: HashMap<String, Vec<FileEntry>>,
    pub tree_loading: HashSet<String>,
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
}

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
            tree_width: 180.0,
            tree_expanded: HashSet::from(["/".to_string()]),
            tree_children: HashMap::new(),
            tree_loading: HashSet::new(),
            transfers: Vec::new(),
            history: Vec::new(),
            sftp_tx,
            started: false,
            applied_home: false,
            dialogs: FmDialogs::default(),
            show_transfers_pane: true,
            transfer_pos: TransferPos::Bottom,
            next_transfer_id: 1,
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
        if !self.started && self.sftp_tx.is_some() {
            self.started = true;
            let cwd = self.cwd.clone();
            self.request_list(&cwd);
            self.reveal_in_tree(&cwd);
        }
    }

    pub fn send(&self, cmd: SftpCmd) -> Result<(), String> {
        let tx = self
            .sftp_tx
            .as_ref()
            .ok_or_else(|| "文件服务尚未就绪".to_string())?;
        tx.send(cmd).map_err(|_| "文件服务连接已关闭".to_string())
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
        if let Err(error) = self.send(SftpCmd::List(path.to_string())) {
            self.tree_loading.remove(path);
            self.loading = false;
            self.error = Some(error);
        }
    }

    pub fn navigate(&mut self, path: &str) {
        let norm = normalize_path(path);
        if norm != self.cwd {
            if !self.history.contains(&self.cwd) {
                self.history.push(self.cwd.clone());
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
            if !self.tree_children.contains_key(&next) {
                self.request_list(&next);
            }
            cur = next;
        }
    }

    /// 展开树节点(点击目录名时:展开 + 进入)
    pub fn expand_tree(&mut self, path: &str) {
        self.tree_expanded.insert(path.to_string());
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
                match result {
                    Ok(mut entries) => {
                        let dirs: Vec<FileEntry> =
                            entries.iter().filter(|e| e.is_dir).cloned().collect();
                        self.tree_children.insert(path.clone(), dirs);
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
                    if let Some(t) =
                        id.and_then(|id| self.transfers.iter_mut().find(|t| t.id == id))
                    {
                        t.finished = true;
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
                    t.done = done;
                    t.total = total;
                } else {
                    self.transfers.push(Transfer {
                        id,
                        op,
                        path,
                        done,
                        total,
                        finished: false,
                        error: None,
                    });
                }
            }
        }
    }

    /// 下载选中的文件到本地路径
    pub fn start_download(&mut self, entry: &FileEntry, local_path: String) {
        let remote = entry.full_path(&self.cwd);
        let id = self.next_transfer_id();
        if let Err(error) = self.send(SftpCmd::Download {
            id,
            remote: remote.clone(),
            local: local_path,
        }) {
            self.error = Some(error);
            return;
        }
        self.transfers.push(Transfer {
            id,
            op: "download",
            path: remote,
            done: 0,
            total: entry.size,
            finished: false,
            error: None,
        });
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
        self.transfers.push(Transfer {
            id,
            op: "download_dir",
            path: remote,
            done: 0,
            total: 0,
            finished: false,
            error: None,
        });
    }

    /// 上传整个本地目录到当前远程目录
    pub fn start_upload_dir(&mut self, local_path: String) {
        let dir_name = local_path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("upload")
            .to_string();
        let remote = join_path(&self.cwd, &dir_name);
        // 估算本地目录大小:遍历收集
        let mut files = Vec::new();
        let total = crate::session::files::walk_files(&local_path, String::new(), &mut files)
            .ok()
            .map(|_| files.iter().map(|(_, s)| *s).sum::<u64>())
            .unwrap_or(0);
        let id = self.next_transfer_id();
        if let Err(error) = self.send(SftpCmd::UploadDir {
            id,
            local: local_path.clone(),
            remote,
        }) {
            self.error = Some(error);
            return;
        }
        self.transfers.push(Transfer {
            id,
            op: "upload_dir",
            path: local_path,
            done: 0,
            total,
            finished: false,
            error: None,
        });
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
        self.transfers.push(Transfer {
            id,
            op: "upload",
            path: local_path,
            done: 0,
            total,
            finished: false,
            error: None,
        });
    }

    fn next_transfer_id(&mut self) -> u64 {
        let id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1).max(1);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_matches_transfer_id_not_operation_order() {
        let mut manager = FileManager::new(None);
        manager.transfers = vec![
            Transfer {
                id: 10,
                op: "download",
                path: "/first".into(),
                done: 0,
                total: 1,
                finished: false,
                error: None,
            },
            Transfer {
                id: 11,
                op: "download",
                path: "/second".into(),
                done: 0,
                total: 1,
                finished: false,
                error: None,
            },
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
    fn failed_enqueue_does_not_create_transfer() {
        let mut manager = FileManager::new(None);
        manager.start_upload("/does/not/exist".into());
        assert!(manager.transfers.is_empty());
        assert_eq!(manager.error.as_deref(), Some("文件服务尚未就绪"));
    }
}
