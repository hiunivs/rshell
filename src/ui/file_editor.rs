//! 在线预览/编辑:独立原生窗口(viewport)的文本编辑器——多文件标签、行号、
//! 语法高亮(syntect)、搜索/替换、保存回传、本地程序打开。
//! 类型判定层级(file_kind.rs):1 执行位 → 2 扩展名 → 4 前 8KB 内容嗅探 → 3 file 命令兜底。

use crate::app::RShellApp;
use crate::config::FileOpenMode;
use crate::file_kind::FileKind;
use crate::session::files::{FileEntry, SftpCmd};
use crate::ui::sidebar::fmt_bytes;
use egui::{Color32, RichText, vec2};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

/// 内容探测字节数(层级 4 嗅探只需要文件头)
const PROBE_BYTES: u64 = 8 * 1024;
/// 超过该大小先弹确认框再加载
const CONFIRM_BYTES: u64 = 10 * 1024 * 1024;
/// 内建编辑器加载上限,超过只能用外部程序/下载
/// 文本本体仍需驻留内存；排版则由下方的分块上限独立控制。
const MAX_EDIT_BYTES: u64 = 16 * 1024 * 1024;
/// 单个排版块的语法高亮上限；超过退化为纯文本布局。
const HIGHLIGHT_MAX_BYTES: usize = 512 * 1024;
/// 虚拟编辑器每个排版块的目标行数。滚动时只创建屏幕附近的块。
const EDITOR_CHUNK_LINES: usize = 96;
/// 单个可见排版块的文本上限。行数少但每行很长时仍按字节拆块。
const EDITOR_CHUNK_BYTES: usize = 64 * 1024;
/// 超长单行无法在不改变内容的前提下跨 TextEdit 分块，交给外部编辑器处理。
const EDITOR_MAX_LINE_BYTES: usize = EDITOR_CHUNK_BYTES;
/// 单个块允许暂时增长到该行数；超过后立即重新分块，避免一次粘贴撑大 Galley。
const EDITOR_CHUNK_MAX_LINES: usize = EDITOR_CHUNK_LINES * 2;
/// 每个块的撤销快照上限；快照只包含一个小块，不再复制整份文件。
const EDITOR_UNDO_LIMIT: usize = 24;

/// 编辑器 viewport 的固定 ID(app 侧聚焦召回也要用)
pub(crate) fn editor_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("rshell_editor")
}

// ---------------------------------------------------------------------------
// 编辑器状态
// ---------------------------------------------------------------------------

/// 编辑器窗口(独立 viewport)整体状态
#[derive(Default)]
pub struct EditorState {
    pub show: bool,
    pub docs: Vec<EditorDoc>,
    pub active: usize,
    /// 用户显式打开文件后，在下一次编辑器 viewport 绘制时召回窗口。
    /// 仅由打开文件动作设置，应用切回前台不会触发。
    pub focus_viewport: bool,
    /// 有未保存文件时拦截窗口关闭后的确认框
    pub confirm_close_all: bool,
    /// 打开搜索栏后下一帧聚焦查询框
    pub focus_search: bool,
    /// 原生关闭帧仍需绘制旧内容，避免交换一帧空画面；绘制完成后再释放文档。
    discard_docs_after_frame: bool,
}

impl EditorState {
    fn activate_doc(&mut self, index: usize) {
        debug_assert!(index < self.docs.len());
        self.active = index;
        self.show = true;
        self.focus_viewport = true;
    }

    fn activate_open_doc(&mut self, session_id: usize, path: &str) -> bool {
        let Some(index) = self
            .docs
            .iter()
            .position(|doc| doc.session_id == session_id && doc.path == path)
        else {
            return false;
        };
        self.activate_doc(index);
        true
    }

    /// 请求关闭全部文档。返回 true 表示文档已经全部关闭，调用方必须立即
    /// 结束当前帧，不能继续使用此前计算的 active 下标。
    fn request_close_all(&mut self) -> bool {
        if self.docs.iter().any(|doc| doc.dirty) {
            self.confirm_close_all = true;
            return false;
        }
        self.close_all_now();
        true
    }

    fn close_all_now(&mut self) {
        self.docs.clear();
        self.active = 0;
        self.show = false;
        self.focus_viewport = false;
        self.confirm_close_all = false;
        self.focus_search = false;
        self.discard_docs_after_frame = false;
    }

    /// 开始关闭干净窗口。当前帧必须保留文档完成最后一次绘制，否则 eframe 会在
    /// 隐藏命令执行前交换一帧空画面。
    fn begin_clean_window_close(&mut self, remember_open_files: bool) {
        self.show = false;
        self.focus_viewport = false;
        self.confirm_close_all = false;
        self.focus_search = false;
        self.discard_docs_after_frame = !remember_open_files;
    }

    fn finish_viewport_frame(&mut self) {
        if self.discard_docs_after_frame {
            self.close_all_now();
        }
    }
}

/// 一个打开的文件标签
pub struct EditorDoc {
    /// 来源会话的稳定 ID(保存时按它路由,切换标签不影响)
    pub session_id: usize,
    /// 标签中的主机标识；打开文件时固化，避免切换会话后串用当前主机。
    pub host_label: String,
    /// 远程完整路径(发送 ReadFile 时已固化,不受后续目录切换影响)
    pub path: String,
    pub file_name: String,
    /// 语法高亮用的文件名(含扩展名,syntect find_syntax_for_file)
    /// 文本按行分块保存。编辑区只排版可见块，避免整篇 Galley。
    chunks: Vec<EditorChunk>,
    /// 文本代数:编辑/替换后 +1,驱动行号与搜索缓存失效
    pub text_gen: u64,
    /// 是否包含尚未保存的修改。
    pub dirty: bool,
    /// 最近一次确认写入成功的内容基线；不保留完整副本，避免文档内存翻倍。
    saved_len: usize,
    saved_digest: [u8; 32],
    text_len: usize,
    /// 进行中的保存(WriteFile 操作 ID)
    saving: Option<PendingSave>,
    /// 保存完成后自动关闭标签("保存并关闭")
    pub close_after_save: bool,
    pub error: Option<String>,
    /// 文件非 UTF-8,内容经 lossy 转换(保存会改写为 UTF-8)
    pub lossy: bool,
    /// 关闭标签确认(有未保存修改时)
    pub confirm_close: bool,
    pub search: SearchState,
    /// 文本行数由分块增量维护，状态栏每帧读取为 O(1)。
    line_count: usize,
    /// 搜索跳转请求对应的全局行号。
    scroll_to_line: Option<usize>,
}

struct EditorChunk {
    id: u64,
    text: String,
    lines: usize,
    /// 自动换行模式下的已测量高度；只缓存数值，不保留 Galley。
    wrap_height: Option<WrapHeightCache>,
}

struct PendingSave {
    id: u64,
    len: usize,
    digest: [u8; 32],
}

#[derive(Clone, Copy)]
struct WrapHeightCache {
    width_bits: u32,
    font_size_bits: u32,
    height: f32,
}

/// 搜索/替换状态(每标签一份)
#[derive(Default)]
pub struct SearchState {
    pub open: bool,
    pub query: String,
    pub replace: String,
    pub case_sensitive: bool,
    /// 当前匹配在 matches 中的下标(None = 尚未跳转)
    pub current: Option<usize>,
    /// 匹配缓存(text_gen/query/大小写变化即失效)
    matches_cache: Option<MatchesCache>,
}

/// 一次全量匹配的结果缓存
struct MatchesCache {
    text_gen: u64,
    query: String,
    case_sensitive: bool,
    /// 匹配的字节区间
    matches: Vec<(usize, usize)>,
}

impl EditorDoc {
    fn new(session_id: usize, path: String, file_name: String, text: String, lossy: bool) -> Self {
        let line_count = line_count(&text);
        let text_len = text.len();
        let chunks = chunk_text(text);
        let saved_digest = digest_chunks(&chunks);
        Self {
            session_id,
            host_label: String::new(),
            path,
            file_name,
            chunks,
            text_gen: 0,
            dirty: false,
            saved_len: text_len,
            saved_digest,
            text_len,
            saving: None,
            close_after_save: false,
            error: None,
            lossy,
            confirm_close: false,
            search: SearchState::default(),
            line_count,
            scroll_to_line: None,
        }
    }

    fn line_count(&self) -> usize {
        self.line_count
    }

    fn mark_text_changed(&mut self) {
        self.text_gen = self.text_gen.wrapping_add(1);
        self.recount();
        self.refresh_dirty();
    }

    /// 保存成功：以实际发送的快照为新基线。若保存期间又发生编辑，当前内容仍会
    /// 与该快照比较，不会被完成事件错误地标记为已保存。
    fn mark_saved(&mut self, len: usize, digest: [u8; 32]) {
        self.saved_len = len;
        self.saved_digest = digest;
        self.refresh_dirty();
    }

    fn refresh_dirty(&mut self) {
        // 绝大多数输入会先改变长度，此时 O(1) 即可判脏。只有长度重新等于保存
        // 基线（撤销、删回原文、等长替换）时才扫描分块计算摘要。
        self.dirty =
            self.text_len != self.saved_len || digest_chunks(&self.chunks) != self.saved_digest;
    }

    fn full_text(&self) -> String {
        let mut text = String::with_capacity(self.text_len);
        for (i, chunk) in self.chunks.iter().enumerate() {
            if i > 0 {
                text.push('\n');
            }
            text.push_str(&chunk.text);
        }
        text
    }

    fn replace_full_text(&mut self, text: String) {
        self.chunks = chunk_text(text);
        self.mark_text_changed();
    }

    fn recount(&mut self) {
        self.line_count = self.chunks.iter().map(|chunk| chunk.lines).sum();
        self.text_len = self
            .chunks
            .iter()
            .map(|chunk| chunk.text.len())
            .sum::<usize>()
            + self.chunks.len().saturating_sub(1);
    }

    fn line_for_byte(&self, byte: usize) -> usize {
        let mut consumed = 0usize;
        let mut line = 0usize;
        for chunk in &self.chunks {
            let end = consumed + chunk.text.len();
            if byte <= end {
                return line
                    + chunk.text[..byte.saturating_sub(consumed).min(chunk.text.len())]
                        .bytes()
                        .filter(|b| *b == b'\n')
                        .count();
            }
            consumed = end + 1;
            line += chunk.lines;
        }
        self.line_count.saturating_sub(1)
    }

    fn rebalance(&mut self) {
        if self
            .chunks
            .iter()
            .all(|chunk| chunk.lines <= EDITOR_CHUNK_MAX_LINES)
        {
            return;
        }
        let text = self.full_text();
        self.chunks = chunk_text(text);
        self.recount();
    }
}

fn next_chunk_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn chunk_text(text: String) -> Vec<EditorChunk> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut lines = 0usize;
    for line in text.split('\n') {
        let added_bytes = line.len() + usize::from(lines > 0);
        if lines == EDITOR_CHUNK_LINES
            || (lines > 0 && current.len() + added_bytes > EDITOR_CHUNK_BYTES)
        {
            chunks.push(EditorChunk {
                id: next_chunk_id(),
                text: std::mem::take(&mut current),
                lines,
                wrap_height: None,
            });
            lines = 0;
        }
        if lines > 0 {
            current.push('\n');
        }
        current.push_str(line);
        lines += 1;
    }
    chunks.push(EditorChunk {
        id: next_chunk_id(),
        text: current,
        lines: lines.max(1),
        wrap_height: None,
    });
    chunks
}

fn line_count(text: &str) -> usize {
    let n = text.lines().count();
    (n + usize::from(text.ends_with('\n'))).max(1)
}

fn digest_chunks(chunks: &[EditorChunk]) -> [u8; 32] {
    let mut digest = Sha256::new();
    for (index, chunk) in chunks.iter().enumerate() {
        if index > 0 {
            digest.update(b"\n");
        }
        digest.update(chunk.text.as_bytes());
    }
    digest.finalize().into()
}

/// 搜索匹配(字节区间);大小写不敏感用整体小写化实现(常见代码/配置文本足够准确)
fn find_matches(text: &str, query: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    if case_sensitive {
        text.match_indices(query)
            .map(|(i, m)| (i, i + m.len()))
            .collect()
    } else {
        let hay = text.to_lowercase();
        let needle = query.to_lowercase();
        hay.match_indices(&needle)
            .map(|(i, m)| (i, i + m.len()))
            .collect()
    }
}

/// 取当前搜索的匹配集(缓存失效时重算)
fn ensure_matches<'s>(
    text: &str,
    text_gen: u64,
    search: &'s mut SearchState,
) -> &'s [(usize, usize)] {
    let valid = search.matches_cache.as_ref().is_some_and(|c| {
        c.text_gen == text_gen
            && c.query == search.query
            && c.case_sensitive == search.case_sensitive
    });
    if !valid {
        let query = search.query.clone();
        let matches = find_matches(text, &query, search.case_sensitive);
        search.matches_cache = Some(MatchesCache {
            text_gen,
            query,
            case_sensitive: search.case_sensitive,
            matches,
        });
    }
    &search
        .matches_cache
        .as_ref()
        .expect("matches filled")
        .matches
}

// ---------------------------------------------------------------------------
// 预览状态机(与前一版相同,打开时进 EditorState)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReadPurpose {
    /// 只读前 8KB 做类型嗅探
    Probe,
    /// 全量读入内存,准备打开编辑器
    Open,
}

/// 已发出的 ReadFile
pub struct PendingRead {
    pub id: u64,
    pub entry: FileEntry,
    pub path: String,
    pub purpose: ReadPurpose,
}

/// 嗅探无法判定,等待远程 `file` 命令结果(层级 3)
pub struct PendingKind {
    pub id: u64,
    pub entry: FileEntry,
    pub path: String,
    /// Some = 全量内容已取回(Text 则直接开编辑器);None = 只探测过(Text 需再全量读)
    pub data: Option<Vec<u8>>,
}

/// 二进制/过大/错误提示对话框
pub struct PreviewNotice {
    pub title: String,
    pub message: String,
    /// 提供"用本地程序打开/下载"按钮的目标文件
    pub entry: FileEntry,
}

/// 外部程序打开:等待临时下载完成
pub struct PendingExternal {
    pub transfer_id: u64,
    pub local: String,
    pub remote: String,
}

impl RShellApp {
    /// 每帧由 show_file_manager 调用:从 fm 收件箱取请求与事件,驱动预览状态机
    pub(crate) fn drain_file_requests(&mut self) {
        let Some(idx) = self.effective_session_idx() else {
            return;
        };
        let (req_preview, req_external, content, kind, write, ext_done) = {
            let fm = &mut self.sessions[idx].fm;
            let ext_done = self.external_pending.as_ref().and_then(|p| {
                fm.transfers
                    .iter()
                    .find(|t| t.id == p.transfer_id && t.finished)
                    .map(|t| t.error.clone())
            });
            (
                fm.request_preview.take(),
                fm.request_external_open.take(),
                fm.preview_inbox.take(),
                fm.kind_inbox.take(),
                fm.write_inbox.take(),
                ext_done,
            )
        };
        if let Some(entry) = req_preview {
            self.begin_preview(entry);
        }
        if let Some(entry) = req_external {
            self.begin_external_open(entry);
        }
        if let Some((id, _path, result)) = content {
            self.on_file_content(id, result);
        }
        if let Some((id, _path, result)) = kind {
            self.on_kind_detected(id, result);
        }
        if let Some((id, result)) = write {
            self.on_write_done(id, result);
        }
        if let Some(err) = ext_done {
            self.on_external_download_done(err);
        }
    }

    /// 双击/右键"打开":层级 1+2 元数据判定,文本直接读,可疑先探测
    fn begin_preview(&mut self, entry: FileEntry) {
        if self.activate_existing_editor(&entry) {
            return;
        }
        match crate::file_kind::from_entry(&entry) {
            FileKind::Binary => {
                self.preview_notice = Some(PreviewNotice {
                    title: "二进制文件".into(),
                    message: format!("{} 是二进制文件，无法在内建编辑器中查看。", entry.name),
                    entry,
                });
            }
            FileKind::Text => self.guarded_full_read(entry),
            FileKind::Executable | FileKind::Unknown => self.send_probe(entry),
        }
    }

    /// 大小检查后全量读文件
    fn guarded_full_read(&mut self, entry: FileEntry) {
        if entry.size > MAX_EDIT_BYTES {
            self.preview_notice = Some(PreviewNotice {
                title: "文件过大".into(),
                message: format!(
                    "{} 有 {}，超过内建编辑器 {} 的上限。",
                    entry.name,
                    fmt_bytes(entry.size),
                    fmt_bytes(MAX_EDIT_BYTES)
                ),
                entry,
            });
            return;
        }
        if entry.size > CONFIRM_BYTES {
            self.preview_confirm = Some(entry);
            return;
        }
        self.send_full_read(entry);
    }

    fn send_read(&mut self, entry: FileEntry, purpose: ReadPurpose, max_bytes: u64) {
        // begin_preview 后可能经历大文件确认或异步类型探测；发送命令前再次检查，
        // 避免这段时间内文件已由另一入口打开而产生多余的远程读取。
        if self.activate_existing_editor(&entry) {
            return;
        }
        let Some(idx) = self.effective_session_idx() else {
            return;
        };
        let fm = &mut self.sessions[idx].fm;
        let id = fm.next_op_id();
        let path = entry.full_path(&fm.cwd);
        if let Err(e) = fm.send(SftpCmd::ReadFile {
            id,
            path: path.clone(),
            max_bytes,
        }) {
            fm.error = Some(e);
            return;
        }
        self.preview_pending = Some(PendingRead {
            id,
            entry,
            path,
            purpose,
        });
    }

    fn send_probe(&mut self, entry: FileEntry) {
        self.send_read(entry, ReadPurpose::Probe, PROBE_BYTES);
    }

    fn send_full_read(&mut self, entry: FileEntry) {
        // size 来自目录列表,可能过期,多读一点余量;硬上限 MAX_EDIT_BYTES
        let max_bytes = (entry.size + 64 * 1024).clamp(PROBE_BYTES, MAX_EDIT_BYTES);
        self.send_read(entry, ReadPurpose::Open, max_bytes);
    }

    fn send_detect_kind(&mut self, entry: FileEntry, path: String, data: Option<Vec<u8>>) {
        let Some(idx) = self.effective_session_idx() else {
            return;
        };
        let fm = &mut self.sessions[idx].fm;
        let id = fm.next_op_id();
        if let Err(e) = fm.send(SftpCmd::DetectKind {
            id,
            path: path.clone(),
        }) {
            self.preview_notice = Some(PreviewNotice {
                title: "无法判定文件类型".into(),
                message: format!("{}: {e}", entry.name),
                entry,
            });
            return;
        }
        self.kind_pending = Some(PendingKind {
            id,
            entry,
            path,
            data,
        });
    }

    fn on_file_content(&mut self, id: u64, result: Result<Vec<u8>, String>) {
        let Some(p) = self.preview_pending.take() else {
            return;
        };
        if p.id != id {
            self.preview_pending = Some(p);
            return;
        }
        let data = match result {
            Ok(data) => data,
            Err(e) => {
                self.preview_notice = Some(PreviewNotice {
                    title: "读取失败".into(),
                    message: format!("{}: {e}", p.entry.name),
                    entry: p.entry,
                });
                return;
            }
        };
        // 层级 4:内容嗅探(全量读回来的也只嗅探前 8KB)
        let sniffed = crate::file_kind::sniff(&data[..data.len().min(PROBE_BYTES as usize)]);
        match (p.purpose, sniffed) {
            (ReadPurpose::Probe, FileKind::Text) => self.guarded_full_read(p.entry),
            (ReadPurpose::Probe, FileKind::Unknown) => self.send_detect_kind(p.entry, p.path, None),
            (ReadPurpose::Probe, FileKind::Executable | FileKind::Binary) => {
                self.preview_notice = Some(binary_notice(&p.entry, sniffed));
            }
            (ReadPurpose::Open, FileKind::Text) => self.open_editor(&p.entry, p.path, data, false),
            (ReadPurpose::Open, FileKind::Unknown) => {
                self.send_detect_kind(p.entry, p.path, Some(data));
            }
            (ReadPurpose::Open, FileKind::Executable | FileKind::Binary) => {
                self.preview_notice = Some(binary_notice(&p.entry, sniffed));
            }
        }
    }

    fn on_kind_detected(&mut self, id: u64, result: Result<String, String>) {
        let Some(k) = self.kind_pending.take() else {
            return;
        };
        if k.id != id {
            self.kind_pending = Some(k);
            return;
        }
        match result {
            Ok(mime) => match crate::file_kind::from_mime(&mime) {
                FileKind::Text => {
                    if let Some(data) = k.data {
                        self.open_editor(&k.entry, k.path, data, true);
                    } else {
                        self.guarded_full_read(k.entry);
                    }
                }
                kind => {
                    self.preview_notice = Some(binary_notice(&k.entry, kind));
                }
            },
            // 远端没有 file 命令:非 UTF-8 内容按 lossy 文本打开(可编辑,保存转 UTF-8)
            Err(_) => {
                if let Some(data) = k.data {
                    self.open_editor(&k.entry, k.path, data, true);
                } else {
                    self.guarded_full_read(k.entry);
                }
            }
        }
    }

    /// 打开(或聚焦)一个编辑标签
    fn open_editor(&mut self, entry: &FileEntry, path: String, data: Vec<u8>, lossy_hint: bool) {
        let session_id = self
            .effective_session_idx()
            .map(|idx| self.sessions[idx].id)
            .unwrap_or(0);
        // 两个读取请求可能在首个请求完成前同时发出，完成时再做一次并发兜底。
        if self.editor.activate_open_doc(session_id, &path) {
            return;
        }
        let host_label = self.editor_host_label(session_id);
        let (text, lossy) = match String::from_utf8(data) {
            Ok(text) => (text, false),
            Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
        };
        if text
            .split('\n')
            .any(|line| line.len() > EDITOR_MAX_LINE_BYTES)
        {
            self.preview_notice = Some(PreviewNotice {
                title: "单行内容过长".into(),
                message: format!(
                    "{} 含有超过 {} 的单行文本。为避免内建编辑器产生大块排版内存，请使用外部编辑器打开。",
                    entry.name,
                    fmt_bytes(EDITOR_MAX_LINE_BYTES as u64)
                ),
                entry: entry.clone(),
            });
            return;
        }
        let mut doc = EditorDoc::new(
            session_id,
            path,
            entry.name.clone(),
            text,
            lossy || lossy_hint,
        );
        doc.host_label = host_label;
        self.editor.docs.push(doc);
        self.editor.activate_doc(self.editor.docs.len() - 1);
    }

    fn activate_existing_editor(&mut self, entry: &FileEntry) -> bool {
        let Some(idx) = self.effective_session_idx() else {
            return false;
        };
        let session_id = self.sessions[idx].id;
        let path = entry.full_path(&self.sessions[idx].fm.cwd);
        self.editor.activate_open_doc(session_id, &path)
    }

    fn editor_host_label(&self, session_id: usize) -> String {
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        else {
            return String::new();
        };
        match session.kind {
            crate::app::SessionKind::Local => "本机".into(),
            crate::app::SessionKind::Ssh => session
                .ssh_params()
                .map(|params| ssh_editor_host_label(&session.title, &params.user, &session.host))
                .unwrap_or_else(|| session.host.clone()),
            _ => session.host.clone(),
        }
    }

    fn on_write_done(&mut self, id: u64, result: Result<(), String>) {
        let Some(doc_idx) = self
            .editor
            .docs
            .iter()
            .position(|doc| doc.saving.as_ref().is_some_and(|saving| saving.id == id))
        else {
            return;
        };
        let doc = &mut self.editor.docs[doc_idx];
        let saving = doc.saving.take().expect("matching save checked");
        let close_after = doc.close_after_save;
        match result {
            Ok(()) => {
                doc.mark_saved(saving.len, saving.digest);
                doc.close_after_save = false;
            }
            Err(e) => {
                doc.error = Some(format!("保存失败:{e}"));
                return;
            }
        }
        if close_after {
            self.remove_editor_doc(doc_idx);
        }
    }

    fn save_editor_doc(&mut self, doc_idx: usize) {
        let Some(doc) = self.editor.docs.get(doc_idx) else {
            return;
        };
        let session_id = doc.session_id;
        let Some(idx) = self.sessions.iter().position(|s| s.id == session_id) else {
            self.editor.docs[doc_idx].error = Some("会话已关闭，无法保存".into());
            return;
        };
        let fm = &mut self.sessions[idx].fm;
        let id = fm.next_op_id();
        let doc = &mut self.editor.docs[doc_idx];
        let data = doc.full_text().into_bytes();
        let len = data.len();
        let digest: [u8; 32] = Sha256::digest(&data).into();
        let result = fm.send(SftpCmd::WriteFile {
            id,
            path: doc.path.clone(),
            data,
        });
        match result {
            Ok(()) => {
                doc.saving = Some(PendingSave { id, len, digest });
                doc.error = None;
            }
            Err(e) => doc.error = Some(format!("保存失败:{e}")),
        }
    }

    fn remove_editor_doc(&mut self, doc_idx: usize) {
        self.editor.docs.remove(doc_idx);
        if self.editor.active >= self.editor.docs.len() {
            self.editor.active = self.editor.docs.len().saturating_sub(1);
        }
        if self.editor.docs.is_empty() {
            self.editor.show = false;
        }
    }

    // -----------------------------------------------------------------------
    // 外部程序打开
    // -----------------------------------------------------------------------

    fn begin_external_open(&mut self, entry: FileEntry) {
        let Some(idx) = self.effective_session_idx() else {
            return;
        };
        let is_local = matches!(self.sessions[idx].kind, crate::app::SessionKind::Local);
        let remote = entry.full_path(&self.sessions[idx].fm.cwd);
        if is_local {
            // 本机文件:直接打开,编辑即时生效,无需回传
            if let Err(e) = open_with_program(&self.config.transfers, &remote) {
                self.sessions[idx].fm.error = Some(e);
            }
            return;
        }
        // SSH:先下载到临时目录,完成后打开并监视回传
        let session_id = self.sessions[idx].id;
        let dir = std::env::temp_dir()
            .join("rshell")
            .join(session_id.to_string());
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.sessions[idx].fm.error = Some(format!("创建临时目录失败:{e}"));
            return;
        }
        let local_path = dir.join(&entry.name).to_string_lossy().into_owned();
        let fm = &mut self.sessions[idx].fm;
        if let Some(transfer_id) = fm.start_download(&entry, local_path.clone()) {
            self.external_pending = Some(PendingExternal {
                transfer_id,
                local: local_path,
                remote,
            });
        }
    }

    fn on_external_download_done(&mut self, error: Option<String>) {
        let Some(p) = self.external_pending.take() else {
            return;
        };
        if let Some(e) = error {
            self.sessions_error_notice(&p.remote, e);
            return;
        }
        if let Err(e) = open_with_program(&self.config.transfers, &p.local) {
            self.sessions_error_notice(&p.remote, e);
            return;
        }
        // 监视临时文件,保存后自动传回远程
        let Some(idx) = self.effective_session_idx() else {
            return;
        };
        if let Some(tx) = self.sessions[idx].fm.sftp_tx.clone() {
            spawn_external_watcher(tx, p.local, p.remote);
        }
    }

    fn sessions_error_notice(&mut self, path: &str, error: String) {
        if let Some(idx) = self.effective_session_idx() {
            self.sessions[idx].fm.error = Some(format!("{path}: {error}"));
        }
    }

    // -----------------------------------------------------------------------
    // 独立编辑器窗口(viewport)
    // -----------------------------------------------------------------------

    pub(crate) fn show_editor_viewport(&mut self, ui: &mut egui::Ui) {
        if !self.editor.show || self.editor.docs.is_empty() {
            return;
        }
        let ctx = ui.ctx().clone();
        let focus_viewport = std::mem::take(&mut self.editor.focus_viewport);
        // immediate 变体:回调无 Send/Sync 约束,可直接借用 &mut self
        // (代价是父子窗口重绘耦合,对文本编辑器可接受)
        ctx.show_viewport_immediate(
            editor_viewport_id(),
            egui::ViewportBuilder::default()
                .with_title("RShell 编辑器")
                .with_inner_size([960.0, 700.0])
                .with_min_inner_size([520.0, 360.0]),
            |vp_ui, _class| {
                // immediate viewport 与主窗口共享 Context/字体系统，不重复 set_fonts。
                let vp_ctx = vp_ui.ctx().clone();
                if focus_viewport {
                    // 这是用户从文件管理器执行“打开文件”的直接结果，因此允许
                    // 恢复最小化窗口并取得焦点。普通 App 激活不会走这条路径。
                    vp_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    vp_ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    vp_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                let style_flag = egui::Id::new("editor_style_applied");
                let wanted_style = (self.config.theme_mode, self.config.color_scheme);
                let style_current = vp_ctx.data_mut(|data| {
                    data.get_temp::<(crate::config::ThemeMode, crate::config::ColorScheme)>(
                        style_flag,
                    )
                });
                if style_current != Some(wanted_style) {
                    vp_ctx.set_theme(self.theme_pref);
                    crate::ui::settings::apply_color_scheme(&vp_ctx, self.config.color_scheme);
                    vp_ctx.data_mut(|data| data.insert_temp(style_flag, wanted_style));
                }
                self.editor_ui(vp_ui);
            },
        );
        self.editor.finish_viewport_frame();
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = &ui.ctx().clone();
        // viewport 的关闭/文件事件可能在两次绘制之间清空文档。任何下标计算
        // 前先把空状态正规化，避免 `len() - 1` 和旧 active 越界。
        if self.editor.docs.is_empty() {
            self.editor.close_all_now();
            return;
        }
        // 原生窗口关闭请求:有未保存文件时拦截并弹确认
        if ctx.input(|i| i.viewport().close_requested()) {
            // 由应用统一接管关闭：先阻止原生窗口立即销毁。直接放行会让 immediate
            // viewport 在主窗口下一帧绘制时短暂重建，造成关闭过程闪动。
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.editor.docs.iter().any(|d| d.dirty) {
                self.editor.confirm_close_all = true;
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.editor
                    .begin_clean_window_close(self.config.editor.remember_open_files);
            }
        }
        // 快捷键
        let (want_save, want_find, want_close_tab) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::S),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::F),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::W),
            )
        });
        let active = self.editor.active.min(self.editor.docs.len() - 1);
        if want_find {
            self.editor.docs[active].search.open = true;
            self.editor.focus_search = true;
        }
        if want_save {
            self.save_editor_doc(active);
        }
        let mut close_tab: Option<usize> = if want_close_tab { Some(active) } else { None };
        let mut close_all_requested = false;

        // 菜单栏(文件 / 搜索)
        egui::Panel::top("editor_menubar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("文件", |ui| {
                    if ui.button("保存        ⌘S").clicked() {
                        ui.close();
                        self.save_editor_doc(active);
                    }
                    if ui.button("关闭当前标签  ⌘W").clicked() {
                        ui.close();
                        close_tab = Some(active);
                    }
                    ui.separator();
                    if ui.button("全部关闭").clicked() {
                        ui.close();
                        // egui 的菜单回调仍处于当前绘制帧，不能在这里直接清空
                        // docs；否则后续搜索栏/状态栏会用旧 active 访问空列表。
                        close_all_requested = true;
                    }
                });
                ui.menu_button("搜索", |ui| {
                    if ui.button("查找 / 替换…  ⌘F").clicked() {
                        ui.close();
                        self.editor.docs[active].search.open = true;
                        self.editor.focus_search = true;
                    }
                    if ui.button("查找下一个").clicked() {
                        ui.close();
                        self.search_step(ctx, 1);
                    }
                    if ui.button("查找上一个").clicked() {
                        ui.close();
                        self.search_step(ctx, -1);
                    }
                });
            });
        });

        if close_all_requested && self.editor.request_close_all() {
            return;
        }

        // 搜索/替换栏
        if self.editor.docs[active].search.open {
            egui::Panel::top("editor_searchbar").show(ui, |ui| {
                self.search_bar_ui(ui, active);
            });
        }

        // 状态栏
        egui::Panel::bottom("editor_statusbar").show(ui, |ui| {
            let doc = &self.editor.docs[active];
            ui.horizontal(|ui| {
                if let Some(error) = &doc.error {
                    ui.colored_label(Color32::from_rgb(0xD8, 0x3A, 0x3A), error);
                } else if doc.saving.is_some() {
                    ui.label("保存中…");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let lines = doc.line_count();
                    let bytes = doc.text_len;
                    let mut info = format!("{lines} 行 · {}", fmt_bytes(bytes as u64));
                    if doc.lossy {
                        info.push_str(" · 非 UTF-8(保存转码)");
                    }
                    ui.label(RichText::new(info).weak().small());
                    if doc.dirty {
                        ui.label(
                            RichText::new("● 未保存")
                                .color(Color32::from_rgb(0xD9, 0x78, 0x32))
                                .small(),
                        );
                    }
                });
            });
        });

        // 标签条 + 编辑区
        egui::CentralPanel::default().show(ui, |ui| {
            // 标签条(自绘,样式与主窗口会话标签一致:[× 文件名],块间用间隙分隔)
            egui::ScrollArea::horizontal()
                .id_salt("editor_tabs")
                .auto_shrink([false, false])
                .max_height(28.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for i in 0..self.editor.docs.len() {
                            match editor_tab(ui, &self.editor.docs[i], i == active) {
                                TabAction::Activate => self.editor.active = i,
                                TabAction::Close => close_tab = Some(i),
                                TabAction::None => {}
                            }
                        }
                    });
                });

            // 分块虚拟编辑区：只创建视口附近的块，文件长度不再决定每帧排版量。
            let font_id = crate::terminal_font_id(self.config.ssh_terminal.font_size as f32);
            let ext = self.editor.docs[active].file_name.clone();
            let jump_line = self.editor.docs[active].scroll_to_line.take();
            let auto_wrap = self.config.editor.auto_wrap;
            let doc = &mut self.editor.docs[active];
            bridge_focused_chunk_boundary(ui.ctx(), doc);
            let (changed, needs_rebalance) =
                show_editor_document(ui, doc, &ext, &font_id, auto_wrap, jump_line);
            if changed {
                doc.mark_text_changed();
            }
            if needs_rebalance {
                doc.rebalance();
            }
        });

        // 关闭标签(未保存则确认)
        if let Some(i) = close_tab {
            if self.editor.docs[i].dirty {
                self.editor.docs[i].confirm_close = true;
            } else {
                self.remove_editor_doc(i);
            }
        }
        // 单标签未保存确认
        if active < self.editor.docs.len() && self.editor.docs[active].confirm_close {
            let (mut save_close, mut discard, mut cancel) = (false, false, false);
            egui::Window::new("未保存的修改")
                .id(egui::Id::new("editor_confirm_close_tab"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .frame(crate::ui::dialog::modal_frame(ctx))
                .show(ctx, |ui| {
                    ui.label(format!(
                        "「{}」有未保存的修改。",
                        self.editor.docs[active].file_name
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("保存并关闭").clicked() {
                            save_close = true;
                        }
                        if ui.button("放弃修改").clicked() {
                            discard = true;
                        }
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                    });
                });
            if active < self.editor.docs.len() {
                if save_close {
                    self.editor.docs[active].close_after_save = true;
                    self.editor.docs[active].confirm_close = false;
                    self.save_editor_doc(active);
                } else if discard {
                    self.remove_editor_doc(active);
                } else if cancel {
                    self.editor.docs[active].confirm_close = false;
                }
            }
        }
        // 窗口关闭确认(全部标签)
        if self.editor.confirm_close_all {
            let (mut discard_all, mut cancel) = (false, false);
            let dirty_count = self.editor.docs.iter().filter(|d| d.dirty).count();
            egui::Window::new("未保存的修改")
                .id(egui::Id::new("editor_confirm_close_all"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .frame(crate::ui::dialog::modal_frame(ctx))
                .show(ctx, |ui| {
                    ui.label(format!(
                        "有 {dirty_count} 个文件未保存，关闭窗口将丢失这些修改。"
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("全部放弃并关闭").clicked() {
                            discard_all = true;
                        }
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                    });
                });
            if discard_all {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.editor.close_all_now();
            } else if cancel {
                self.editor.confirm_close_all = false;
            }
        }
    }

    // -----------------------------------------------------------------------
    // 搜索/替换
    // -----------------------------------------------------------------------

    /// 搜索栏(查找输入、计数、上下跳转、大小写、替换)
    fn search_bar_ui(&mut self, ui: &mut egui::Ui, active: usize) {
        let ctx = ui.ctx().clone();
        let mut close = false;
        let mut step = 0i32;
        let mut replace_one = false;
        let mut replace_all = false;
        let query_id = egui::Id::new("editor_search_query");
        ui.horizontal(|ui| {
            ui.label("查找:");
            let doc = &mut self.editor.docs[active];
            let resp = ui.add(
                egui::TextEdit::singleline(&mut doc.search.query)
                    .id(query_id)
                    .desired_width(180.0),
            );
            if self.editor.focus_search {
                resp.request_focus();
                self.editor.focus_search = false;
            }
            let text = doc.full_text();
            let count = ensure_matches(&text, doc.text_gen, &mut doc.search).len();
            let current = doc.search.current;
            if !doc.search.query.is_empty() {
                let label = match current {
                    Some(i) if count > 0 => format!("{}/{}", i + 1, count),
                    _ => format!("0/{count}"),
                };
                ui.label(RichText::new(label).weak().small());
            }
            if ui
                .add_enabled(count > 0, egui::Button::new("↑").small())
                .on_hover_text("上一个")
                .clicked()
            {
                step = -1;
            }
            if ui
                .add_enabled(count > 0, egui::Button::new("↓").small())
                .on_hover_text("下一个")
                .clicked()
            {
                step = 1;
            }
            ui.checkbox(&mut doc.search.case_sensitive, "大小写");
            ui.separator();
            ui.label("替换为:");
            ui.add(egui::TextEdit::singleline(&mut doc.search.replace).desired_width(140.0));
            if ui
                .add_enabled(count > 0, egui::Button::new("替换"))
                .clicked()
            {
                replace_one = true;
            }
            if ui
                .add_enabled(count > 0, egui::Button::new("全部替换"))
                .clicked()
            {
                replace_all = true;
            }
            if ui.button("×").on_hover_text("关闭搜索").clicked() {
                close = true;
            }
        });
        if step != 0 {
            self.search_step(&ctx, step);
        }
        if replace_one {
            self.replace_current(&ctx, active);
        }
        if replace_all {
            self.replace_all(active);
        }
        if close {
            self.editor.docs[active].search.open = false;
        }
        // Esc 关闭搜索栏(焦点在查询框时)
        if self.editor.docs[active].search.open
            && ui
                .ctx()
                .input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.editor.docs[active].search.open = false;
        }
    }

    /// 查找下一个(step=1)/上一个(step=-1),循环跳转并把光标/选区移到位
    fn search_step(&mut self, ctx: &egui::Context, step: i32) {
        if self.editor.docs.is_empty() {
            return;
        }
        let active = self.editor.active.min(self.editor.docs.len() - 1);
        let doc = &mut self.editor.docs[active];
        let text = doc.full_text();
        let matches = ensure_matches(&text, doc.text_gen, &mut doc.search).to_vec();
        if matches.is_empty() {
            return;
        }
        let n = matches.len();
        let next = match doc.search.current {
            None => 0,
            Some(i) => (i as i32 + step).rem_euclid(n as i32) as usize,
        };
        doc.search.current = Some(next);
        doc.scroll_to_line = Some(doc.line_for_byte(matches[next].0));
        ctx.request_repaint();
    }

    fn replace_current(&mut self, ctx: &egui::Context, active: usize) {
        let doc = &mut self.editor.docs[active];
        let mut text = doc.full_text();
        let matches = ensure_matches(&text, doc.text_gen, &mut doc.search).to_vec();
        if matches.is_empty() {
            return;
        }
        let idx = doc.search.current.unwrap_or(0).min(matches.len() - 1);
        let (start, end) = matches[idx];
        text.replace_range(start..end, &doc.search.replace);
        doc.replace_full_text(text);
        // 替换后重算匹配并停留在原位置附近
        let text = doc.full_text();
        let new_matches = ensure_matches(&text, doc.text_gen, &mut doc.search).to_vec();
        if !new_matches.is_empty() {
            let idx = idx.min(new_matches.len() - 1);
            doc.search.current = Some(idx);
            doc.scroll_to_line = Some(doc.line_for_byte(new_matches[idx].0));
            ctx.request_repaint();
        } else {
            doc.search.current = None;
        }
    }

    fn replace_all(&mut self, active: usize) {
        let doc = &mut self.editor.docs[active];
        let mut text = doc.full_text();
        let matches = ensure_matches(&text, doc.text_gen, &mut doc.search).to_vec();
        // 逆序替换,前面的字节区间不受长度变化影响
        for (start, end) in matches.iter().rev() {
            text.replace_range(*start..*end, &doc.search.replace);
        }
        if !matches.is_empty() {
            doc.replace_full_text(text);
            doc.search.current = None;
        }
    }

    /// 大文件确认 + 二进制/错误提示对话框
    pub(crate) fn show_preview_dialogs(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // 大文件确认
        if let Some(entry) = self.preview_confirm.take() {
            let mut open_in_editor = false;
            let mut cancel = false;
            let mut title_close = false;
            egui::Window::new("文件较大")
                .id(egui::Id::new("preview_confirm_large"))
                .collapsible(false)
                .title_bar(false)
                .resizable(false)
                .pivot(egui::Align2::CENTER_CENTER)
                .default_pos(ctx.content_rect().center())
                .fixed_size([360.0, 142.0])
                .frame(crate::ui::dialog::modal_frame(&ctx))
                .show(&ctx, |ui| {
                    ui.set_width(360.0);
                    title_close = crate::ui::dialog::title_bar_with_height(
                        ui,
                        "文件较大",
                        ui.available_width(),
                        32.0,
                    );
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(16, 14))
                        .show(ui, |ui| {
                            crate::ui::dialog::polish_compact(ui);
                            ui.add(
                                egui::Label::new(format!(
                                    "{} 有 {}，加载可能需要一些时间。仍要打开吗？",
                                    entry.name,
                                    fmt_bytes(entry.size)
                                ))
                                .wrap(),
                            );
                            ui.add_space(10.0);
                            ui.horizontal_centered(|ui| {
                                if crate::ui::dialog::compact_primary_button(ui, "打开").clicked()
                                {
                                    open_in_editor = true;
                                }
                                if ui
                                    .add_sized([56.0, 28.0], egui::Button::new("取消"))
                                    .clicked()
                                {
                                    cancel = true;
                                }
                            });
                        });
                });
            if open_in_editor {
                self.send_full_read(entry);
            } else {
                let _ = cancel || title_close;
            }
        }
        // 二进制/错误提示
        if self.preview_notice.is_some() {
            let notice = self.preview_notice.take().expect("notice checked above");
            let mut keep = true;
            let mut action = None;
            let mut title_close = false;
            egui::Window::new(&notice.title)
                .id(egui::Id::new("preview_notice"))
                .collapsible(false)
                .title_bar(false)
                .resizable(false)
                .pivot(egui::Align2::CENTER_CENTER)
                .default_pos(ctx.content_rect().center())
                .fixed_size([400.0, 168.0])
                .frame(crate::ui::dialog::modal_frame(&ctx))
                .show(&ctx, |ui| {
                    ui.set_width(400.0);
                    title_close = crate::ui::dialog::title_bar_with_height(
                        ui,
                        &notice.title,
                        ui.available_width(),
                        32.0,
                    );
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(16, 14))
                        .show(ui, |ui| {
                            crate::ui::dialog::polish_compact(ui);
                            ui.add(egui::Label::new(&notice.message).wrap());
                            ui.add_space(12.0);
                            ui.horizontal_centered(|ui| {
                                if crate::ui::dialog::primary_button(ui, "用本地程序打开").clicked()
                                {
                                    action = Some(NoticeAction::External);
                                }
                                if ui
                                    .add_sized([72.0, 28.0], egui::Button::new("下载…"))
                                    .clicked()
                                {
                                    action = Some(NoticeAction::Download);
                                }
                                if ui
                                    .add_sized([56.0, 28.0], egui::Button::new("关闭"))
                                    .clicked()
                                {
                                    keep = false;
                                }
                            });
                        });
                });
            keep &= !title_close;
            match action {
                Some(NoticeAction::External) => {
                    let entry = notice.entry.clone();
                    self.begin_external_open(entry);
                }
                Some(NoticeAction::Download) => {
                    let entry = notice.entry.clone();
                    self.download_entry_via_dialog(entry);
                }
                None => {}
            }
            if keep && action.is_none() {
                self.preview_notice = Some(notice);
            }
        }
    }

    /// 提示框里的"下载…":复用下载路径设置(每次询问/指定目录)
    fn download_entry_via_dialog(&mut self, entry: FileEntry) {
        let Some(idx) = self.effective_session_idx() else {
            return;
        };
        let ts = self.config.transfers.clone();
        let fm = &mut self.sessions[idx].fm;
        if entry.is_dir {
            if let Some(path) =
                crate::ui::file_manager::resolve_dir_download_target(&ts, &entry.name)
            {
                fm.start_download_dir(&entry, path);
            }
        } else if let Some(path) =
            crate::ui::file_manager::resolve_file_download_target(&ts, &entry.name)
        {
            fm.start_download(&entry, path);
        }
    }
}

/// 标签交互结果
enum TabAction {
    None,
    Activate,
    Close,
}

/// 自绘编辑器标签:[× 文件名(● 脏标记)],与主窗口会话标签(tab_bar.rs)同风格。
/// × 在标签左侧、与文字同色,悬停变红;中键点击也可关闭。
fn editor_tab(ui: &mut egui::Ui, doc: &EditorDoc, selected: bool) -> TabAction {
    use egui::{Align2, CursorIcon, FontId, Pos2, Sense, Stroke, StrokeKind, vec2};
    let font = FontId::proportional(13.0);
    let tab_label = editor_tab_label(doc);
    let text_color = if selected {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    let text_w = ui.fonts_mut(|f| {
        f.layout_no_wrap(tab_label.clone(), font.clone(), text_color)
            .size()
            .x
    });
    let pad_l = 5.0;
    let x_box = 14.0; // × 点击区
    let gap = 3.0;
    let dot_w = if doc.dirty { 13.0 } else { 0.0 };
    let pad_r = 8.0;
    let tab_h = 22.0;
    let tab_w = pad_l + x_box + gap + text_w + dot_w + pad_r;

    let (rect, resp) = ui.allocate_exact_size(vec2(tab_w, tab_h), Sense::click());
    let bg = if selected {
        ui.visuals().selection.bg_fill
    } else if resp.hovered() {
        ui.visuals().faint_bg_color
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect(
        rect,
        egui::CornerRadius::same(4),
        bg,
        Stroke::NONE,
        StrokeKind::Inside,
    );
    // 左侧 × 区(后注册→同层命中赢,吃掉该区域的点击=关闭)
    let x_rect = egui::Rect::from_min_size(
        Pos2::new(rect.left() + pad_l, rect.top() + (tab_h - x_box) * 0.5),
        vec2(x_box, x_box),
    );
    let x_resp = ui
        .interact(
            x_rect,
            ui.id().with(("editor_tab_x", &doc.path)),
            Sense::click(),
        )
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("关闭标签");
    let x_color = if x_resp.hovered() {
        Color32::from_rgb(0xE8, 0x4C, 0x3D)
    } else {
        text_color
    };
    crate::ui::icons::Icon::X.paint_at(
        ui,
        egui::Rect::from_center_size(x_rect.center(), vec2(11.0, 11.0)),
        x_color,
    );
    // 文件名
    let name_x = rect.left() + pad_l + x_box + gap;
    ui.painter().text(
        Pos2::new(name_x, rect.center().y),
        Align2::LEFT_CENTER,
        &tab_label,
        font,
        text_color,
    );
    // 脏标记 ●:亮橙色,明暗主题下都与标签背景拉开对比
    if doc.dirty {
        ui.painter().text(
            Pos2::new(name_x + text_w + 4.0, rect.center().y),
            Align2::LEFT_CENTER,
            "●",
            FontId::proportional(10.0),
            Color32::from_rgb(0xFF, 0x9F, 0x0A),
        );
    }
    resp.clone()
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(&doc.path);
    if x_resp.clicked()
        || x_resp.clicked_by(egui::PointerButton::Middle)
        || resp.clicked_by(egui::PointerButton::Middle)
    {
        TabAction::Close
    } else if resp.clicked() {
        TabAction::Activate
    } else {
        TabAction::None
    }
}

fn editor_tab_label(doc: &EditorDoc) -> String {
    if doc.host_label.is_empty() {
        doc.file_name.clone()
    } else {
        format!("{} · {}", doc.host_label, doc.file_name)
    }
}

fn ssh_editor_host_label(session_title: &str, user: &str, host: &str) -> String {
    session_title
        .strip_prefix(user)
        .and_then(|title| title.strip_prefix('@'))
        .filter(|target| !target.is_empty())
        .unwrap_or(host)
        .to_owned()
}

enum NoticeAction {
    External,
    Download,
}

fn binary_notice(entry: &FileEntry, kind: FileKind) -> PreviewNotice {
    let (title, what) = match kind {
        FileKind::Executable => ("可执行文件", "可执行程序"),
        _ => ("二进制文件", "二进制文件"),
    };
    PreviewNotice {
        title: title.into(),
        message: format!("{} 是{what}，无法在内建编辑器中查看。", entry.name),
        entry: entry.clone(),
    }
}

/// 用系统默认/自定义程序打开本地文件
fn open_with_program(ts: &crate::config::TransferSettings, path: &str) -> Result<(), String> {
    let custom = ts.open_mode == FileOpenMode::CustomProgram && !ts.open_program.trim().is_empty();
    if custom {
        let program = ts.open_program.trim();
        #[cfg(target_os = "macos")]
        if program.ends_with(".app") {
            return std::process::Command::new("open")
                .args(["-a", program, path])
                .spawn()
                .map(|_| ())
                .map_err(|e| format!("无法启动 {program}: {e}"));
        }
        return std::process::Command::new(program)
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("无法启动 {program}: {e}"));
    }
    open_system_default(path)
}

#[cfg(target_os = "macos")]
fn open_system_default(path: &str) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开文件: {e}"))
}

#[cfg(target_os = "linux")]
fn open_system_default(path: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开文件: {e}"))
}

#[cfg(target_os = "windows")]
fn open_system_default(path: &str) -> Result<(), String> {
    std::process::Command::new("cmd")
        .args(["/c", "start", "", path])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开文件: {e}"))
}

/// 监视外部程序打开的临时文件:mtime 变化即重新上传回远程。
/// 会话断开(sftp 通道关闭)或临时文件被删除时线程退出。
fn spawn_external_watcher(sftp_tx: crate::session::files::SftpTx, local: String, remote: String) {
    /// 监视回传的传输 ID 从高段起分配,避免与 UI 侧的传输 ID 冲突
    static WATCH_TRANSFER_ID: AtomicU64 = AtomicU64::new(1 << 40);
    std::thread::spawn(move || {
        let mut last_mtime = std::fs::metadata(&local).and_then(|m| m.modified()).ok();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let Ok(meta) = std::fs::metadata(&local) else {
                break; // 临时文件被删除
            };
            let mtime = meta.modified().ok();
            if mtime == last_mtime {
                continue;
            }
            last_mtime = mtime;
            let id = WATCH_TRANSFER_ID.fetch_add(1, Ordering::Relaxed);
            if sftp_tx
                .blocking_send(SftpCmd::Upload {
                    id,
                    local: local.clone(),
                    remote: remote.clone(),
                })
                .is_err()
            {
                break; // 会话已断开
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 语法高亮(syntect,主流方案;>512KB 自动退化为纯文本)
// ---------------------------------------------------------------------------

fn show_editor_document(
    ui: &mut egui::Ui,
    doc: &mut EditorDoc,
    file_name: &str,
    font_id: &egui::FontId,
    auto_wrap: bool,
    jump_line: Option<usize>,
) -> (bool, bool) {
    let row_height = ui.fonts_mut(|fonts| fonts.row_height(font_id));
    let available_width = ui.available_width();
    let digits = doc.line_count.max(1).ilog10() as usize + 1;
    let digit_width = ui.fonts_mut(|fonts| fonts.glyph_width(font_id, '0'));
    let gutter_width = (digits as f32 * digit_width + 12.0).ceil();
    let wrap_width = (available_width - gutter_width - ui.spacing().item_spacing.x).max(24.0);
    let width_bits = wrap_width.to_bits();
    let font_size_bits = font_id.size.to_bits();

    let jump_offset = jump_line.map(|line| {
        if auto_wrap {
            estimated_wrapped_line_offset(
                doc,
                line,
                wrap_width,
                row_height,
                width_bits,
                font_size_bits,
                digit_width,
            )
        } else {
            line as f32 * row_height
        }
    });
    let mut scroll = if auto_wrap {
        egui::ScrollArea::vertical()
    } else {
        egui::ScrollArea::both()
    }
    .id_salt("editor_scroll")
    .auto_shrink([false, false]);
    if let Some(offset) = jump_offset {
        scroll = scroll.vertical_scroll_offset(offset);
    }

    let mut changed = false;
    let mut needs_rebalance = false;
    scroll.show_viewport(ui, |ui, viewport| {
        if auto_wrap {
            show_wrapped_chunks(
                ui,
                viewport,
                doc,
                file_name,
                font_id,
                row_height,
                gutter_width,
                wrap_width,
                width_bits,
                font_size_bits,
                digit_width,
                &mut changed,
                &mut needs_rebalance,
            );
        } else {
            show_unwrapped_chunks(
                ui,
                viewport,
                doc,
                file_name,
                font_id,
                row_height,
                gutter_width,
                &mut changed,
                &mut needs_rebalance,
            );
        }
    });
    (changed, needs_rebalance)
}

#[allow(clippy::too_many_arguments)]
fn show_unwrapped_chunks(
    ui: &mut egui::Ui,
    viewport: egui::Rect,
    doc: &mut EditorDoc,
    file_name: &str,
    font_id: &egui::FontId,
    row_height: f32,
    gutter_width: f32,
    changed: &mut bool,
    needs_rebalance: &mut bool,
) {
    let content_top = ui.max_rect().top();
    let total_height = doc.line_count as f32 * row_height;
    ui.set_min_size(vec2(ui.available_width(), total_height));
    // viewport 使用内容坐标；加回滚动偏移后得到屏幕中固定不动的行号栏。
    let viewport_left = ui.max_rect().left() + viewport.left();
    let viewport_top = ui.max_rect().top() + viewport.top();
    let gutter_rect = egui::Rect::from_min_size(
        egui::pos2(viewport_left, viewport_top),
        vec2(gutter_width, viewport.height()),
    )
    .intersect(ui.clip_rect());
    ui.painter()
        .rect_filled(gutter_rect, 0.0, ui.visuals().faint_bg_color);
    let text_clip = egui::Rect::from_min_max(
        egui::pos2(gutter_rect.right(), ui.clip_rect().top()),
        ui.clip_rect().max,
    );
    let overscan = row_height * 12.0;
    let visible_top = (viewport.top() - overscan).max(0.0);
    let visible_bottom = viewport.bottom() + overscan;
    let mut first_line = 0usize;
    for chunk in &mut doc.chunks {
        let actual_lines = chunk.lines;
        let chunk_top = first_line as f32 * row_height;
        let chunk_bottom = chunk_top + actual_lines as f32 * row_height;
        if chunk_bottom < visible_top || chunk_top > visible_bottom {
            first_line += actual_lines;
            continue;
        }
        let editor_id = egui::Id::new(("editor_chunk", chunk.id));
        configure_chunk_undo(ui.ctx(), editor_id);
        let rect = egui::Rect::from_min_size(
            egui::pos2(ui.max_rect().left(), content_top + chunk_top),
            vec2(ui.available_width(), actual_lines as f32 * row_height),
        );
        for line_offset in 0..actual_lines {
            let y = rect.top() + (line_offset as f32 + 0.5) * row_height;
            if gutter_rect.y_range().contains(y) {
                ui.painter().text(
                    egui::pos2(gutter_rect.right() - 6.0, y),
                    egui::Align2::RIGHT_CENTER,
                    (first_line + line_offset + 1).to_string(),
                    font_id.clone(),
                    ui.visuals().weak_text_color(),
                );
            }
        }
        let text_rect =
            egui::Rect::from_min_max(egui::pos2(rect.left() + gutter_width, rect.top()), rect.max);
        ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt(("editor_chunk_ui", chunk.id))
                .max_rect(text_rect)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
            |ui| {
                ui.set_clip_rect(text_clip);
                let dark = ui.visuals().dark_mode;
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut chunk.text)
                        .id(editor_id)
                        .font(font_id.clone())
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .desired_rows(actual_lines)
                        .char_limit(EDITOR_CHUNK_BYTES)
                        .layouter(&mut |ui, buf, _wrap| {
                            syntax_layout(ui, buf.as_str(), file_name, dark, font_id, f32::INFINITY)
                        }),
                );
                if resp.changed() {
                    chunk.lines = line_count(&chunk.text);
                    chunk.wrap_height = None;
                    *changed = true;
                }
                *needs_rebalance |= resp.lost_focus() && chunk.lines > EDITOR_CHUNK_MAX_LINES;
            },
        );
        first_line += actual_lines;
    }
    // 行号栏后注册，避免其下方已经滚动过去的 TextEdit 获取点击或拖选。
    let _ = ui.interact(
        gutter_rect,
        ui.id().with("editor_fixed_gutter"),
        egui::Sense::hover(),
    );
}

#[allow(clippy::too_many_arguments)]
fn show_wrapped_chunks(
    ui: &mut egui::Ui,
    viewport: egui::Rect,
    doc: &mut EditorDoc,
    file_name: &str,
    font_id: &egui::FontId,
    row_height: f32,
    gutter_width: f32,
    wrap_width: f32,
    width_bits: u32,
    font_size_bits: u32,
    digit_width: f32,
    changed: &mut bool,
    needs_rebalance: &mut bool,
) {
    ui.spacing_mut().item_spacing.y = 0.0;
    let content_width = ui.available_width();
    let overscan = row_height * 12.0;
    let visible_top = (viewport.top() - overscan).max(0.0);
    let visible_bottom = viewport.bottom() + overscan;
    let mut first_line = 0usize;
    let mut chunk_top = 0.0;

    for chunk in &mut doc.chunks {
        let estimated_height = cached_or_estimated_wrap_height(
            chunk,
            wrap_width,
            row_height,
            width_bits,
            font_size_bits,
            digit_width,
        );
        let chunk_bottom = chunk_top + estimated_height;
        if chunk_bottom < visible_top || chunk_top > visible_bottom {
            ui.allocate_space(vec2(content_width, estimated_height));
            chunk_top += estimated_height;
            first_line += chunk.lines;
            continue;
        }

        let dark = ui.visuals().dark_mode;
        let initial_text = chunk.text.clone();
        let initial_galley = syntax_layout(ui, &initial_text, file_name, dark, font_id, wrap_width);
        let exact_height = initial_galley.size().y.max(row_height);
        let old_height = estimated_height;
        chunk.wrap_height = Some(WrapHeightCache {
            width_bits,
            font_size_bits,
            height: exact_height,
        });
        if (old_height - exact_height).abs() > 0.5 {
            ui.ctx().request_repaint();
        }

        let (rect, _) =
            ui.allocate_exact_size(vec2(content_width, exact_height), egui::Sense::hover());
        let gutter_rect = egui::Rect::from_min_max(
            rect.min,
            egui::pos2(rect.left() + gutter_width, rect.bottom()),
        );
        ui.painter()
            .rect_filled(gutter_rect, 0.0, ui.visuals().faint_bg_color);
        paint_wrapped_gutter(ui, gutter_rect, first_line + 1, &initial_galley, font_id);

        let text_rect = egui::Rect::from_min_max(
            egui::pos2(
                gutter_rect.right() + ui.spacing().item_spacing.x,
                rect.top(),
            ),
            rect.max,
        );
        let editor_id = egui::Id::new(("editor_chunk", chunk.id));
        configure_chunk_undo(ui.ctx(), editor_id);
        ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt(("editor_chunk_wrap_ui", chunk.id))
                .max_rect(text_rect)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
            |ui| {
                let cached_galley = initial_galley.clone();
                let mut layouter = |ui: &egui::Ui,
                                    buf: &dyn egui::TextBuffer,
                                    requested_width: f32| {
                    if buf.as_str() == initial_text && (requested_width - wrap_width).abs() <= 0.5 {
                        cached_galley.clone()
                    } else {
                        syntax_layout(ui, buf.as_str(), file_name, dark, font_id, requested_width)
                    }
                };
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut chunk.text)
                        .id(editor_id)
                        .font(font_id.clone())
                        .frame(egui::Frame::NONE)
                        .desired_width(text_rect.width())
                        .desired_rows(1)
                        .char_limit(EDITOR_CHUNK_BYTES)
                        .layouter(&mut layouter),
                );
                if resp.changed() {
                    chunk.lines = line_count(&chunk.text);
                    chunk.wrap_height = None;
                    *changed = true;
                    ui.ctx().request_repaint();
                }
                *needs_rebalance |= resp.lost_focus() && chunk.lines > EDITOR_CHUNK_MAX_LINES;
            },
        );
        chunk_top += exact_height;
        first_line += chunk.lines;
    }
}

fn paint_wrapped_gutter(
    ui: &egui::Ui,
    rect: egui::Rect,
    first_line: usize,
    galley: &egui::Galley,
    font_id: &egui::FontId,
) {
    let color = ui.visuals().weak_text_color();
    let labels = wrapped_gutter_line_numbers(
        first_line,
        galley.rows.iter().map(|row| row.ends_with_newline),
    );
    for (placed_row, label) in galley.rows.iter().zip(labels) {
        if let Some(line) = label {
            ui.painter().text(
                egui::pos2(
                    rect.right() - 6.0,
                    rect.top() + placed_row.rect().center().y,
                ),
                egui::Align2::RIGHT_CENTER,
                line.to_string(),
                font_id.clone(),
                color,
            );
        }
    }
}

fn wrapped_gutter_line_numbers(
    first_line: usize,
    row_ends: impl IntoIterator<Item = bool>,
) -> Vec<Option<usize>> {
    let mut line = first_line;
    let mut paragraph_start = true;
    row_ends
        .into_iter()
        .map(|ends_with_newline| {
            let label = paragraph_start.then_some(line);
            if ends_with_newline {
                line += 1;
                paragraph_start = true;
            } else {
                paragraph_start = false;
            }
            label
        })
        .collect()
}

fn cached_or_estimated_wrap_height(
    chunk: &EditorChunk,
    wrap_width: f32,
    row_height: f32,
    width_bits: u32,
    font_size_bits: u32,
    digit_width: f32,
) -> f32 {
    if let Some(cache) = chunk.wrap_height
        && cache.width_bits == width_bits
        && cache.font_size_bits == font_size_bits
    {
        return cache.height;
    }
    estimated_visual_rows(&chunk.text, wrap_width, digit_width) as f32 * row_height
}

fn estimated_wrapped_line_offset(
    doc: &EditorDoc,
    target_line: usize,
    wrap_width: f32,
    row_height: f32,
    width_bits: u32,
    font_size_bits: u32,
    digit_width: f32,
) -> f32 {
    let mut offset = 0.0;
    let mut first_line = 0usize;
    for chunk in &doc.chunks {
        if target_line >= first_line + chunk.lines {
            offset += cached_or_estimated_wrap_height(
                chunk,
                wrap_width,
                row_height,
                width_bits,
                font_size_bits,
                digit_width,
            );
            first_line += chunk.lines;
            continue;
        }
        let lines_before = target_line.saturating_sub(first_line);
        if lines_before > 0 {
            let prefix = chunk
                .text
                .split('\n')
                .take(lines_before)
                .collect::<Vec<_>>()
                .join("\n");
            offset += estimated_visual_rows(&prefix, wrap_width, digit_width) as f32 * row_height;
        }
        break;
    }
    offset
}

fn estimated_visual_rows(text: &str, wrap_width: f32, digit_width: f32) -> usize {
    let columns = (wrap_width / digit_width.max(1.0)).floor().max(1.0) as usize;
    text.split('\n')
        .map(|line| {
            let mut cells = 0usize;
            for ch in line.chars() {
                cells += match ch {
                    '\t' => 4 - cells % 4,
                    _ if ch.is_ascii() => 1,
                    _ => 2,
                };
            }
            cells.max(1).div_ceil(columns)
        })
        .sum::<usize>()
        .max(1)
}

fn configure_chunk_undo(ctx: &egui::Context, editor_id: egui::Id) {
    let flag = editor_id.with("bounded_undo");
    if ctx.data_mut(|data| data.get_temp::<bool>(flag).unwrap_or(false)) {
        return;
    }
    let mut state = egui::text_edit::TextEditState::load(ctx, editor_id).unwrap_or_default();
    state.set_undoer(egui::util::undoer::Undoer::with_settings(
        egui::util::undoer::Settings {
            max_undos: EDITOR_UNDO_LIMIT,
            ..Default::default()
        },
    ));
    state.store(ctx, editor_id);
    ctx.data_mut(|data| data.insert_temp(flag, true));
}

/// 分块只是一种存储/渲染细节。光标到达块边缘时先把相邻小块合并，
/// 再让 TextEdit 处理同一个 Backspace/Delete/方向键事件，块边界对用户透明。
fn bridge_focused_chunk_boundary(ctx: &egui::Context, doc: &mut EditorDoc) {
    let key = ctx.input(|input| {
        input.events.iter().find_map(|event| match event {
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if !modifiers.command && !modifiers.alt => Some(*key),
            _ => None,
        })
    });
    let Some(key) = key else { return };
    if !matches!(
        key,
        egui::Key::Backspace | egui::Key::Delete | egui::Key::ArrowUp | egui::Key::ArrowDown
    ) {
        return;
    }

    for idx in 0..doc.chunks.len() {
        let editor_id = egui::Id::new(("editor_chunk", doc.chunks[idx].id));
        if !ctx.memory(|memory| memory.has_focus(editor_id)) {
            continue;
        }
        let Some(mut state) = egui::text_edit::TextEditState::load(ctx, editor_id) else {
            return;
        };
        let Some(range) = state.cursor.char_range() else {
            return;
        };
        if range.primary.index != range.secondary.index {
            return;
        }
        let cursor = range.primary.index.0;
        let text = &doc.chunks[idx].text;
        let before_has_newline = text.chars().take(cursor).any(|ch| ch == '\n');
        let after_has_newline = text.chars().skip(cursor).any(|ch| ch == '\n');
        let wants_previous = matches!(key, egui::Key::Backspace | egui::Key::ArrowUp)
            && !before_has_newline
            && idx > 0;
        let wants_next = matches!(key, egui::Key::Delete | egui::Key::ArrowDown)
            && !after_has_newline
            && idx + 1 < doc.chunks.len();

        if wants_previous {
            let combined_chars =
                doc.chunks[idx - 1].text.chars().count() + 1 + doc.chunks[idx].text.chars().count();
            if combined_chars > EDITOR_CHUNK_BYTES {
                return;
            }
            let previous = doc.chunks.remove(idx - 1);
            let current = &mut doc.chunks[idx - 1];
            let prefix_chars = previous.text.chars().count() + 1;
            current.text = format!("{}\n{}", previous.text, current.text);
            current.lines += previous.lines;
            current.wrap_height = None;
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor + prefix_chars),
                )));
        } else if wants_next {
            let combined_chars =
                doc.chunks[idx].text.chars().count() + 1 + doc.chunks[idx + 1].text.chars().count();
            if combined_chars > EDITOR_CHUNK_BYTES {
                return;
            }
            let next = doc.chunks.remove(idx + 1);
            let current = &mut doc.chunks[idx];
            current.text.push('\n');
            current.text.push_str(&next.text);
            current.lines += next.lines;
            current.wrap_height = None;
        } else {
            return;
        }

        state.set_undoer(egui::util::undoer::Undoer::with_settings(
            egui::util::undoer::Settings {
                max_undos: EDITOR_UNDO_LIMIT,
                ..Default::default()
            },
        ));
        state.store(ctx, editor_id);
        doc.recount();
        return;
    }
}

fn syntax_layout(
    ui: &egui::Ui,
    text: &str,
    file_name: &str,
    dark: bool,
    font_id: &egui::FontId,
    wrap_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = if text.len() > HIGHLIGHT_MAX_BYTES {
        plain_layout_job(text, font_id, ui.visuals().text_color())
    } else {
        syntax_layout_job(text, file_name, dark, font_id, ui.visuals().text_color())
    };
    job.wrap.max_width = wrap_width;
    job.wrap.break_anywhere = wrap_width.is_finite();
    job.keep_trailing_whitespace = true;
    ui.fonts_mut(|f| f.layout_job(job))
}

fn plain_layout_job(text: &str, font_id: &egui::FontId, color: Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: font_id.clone(),
            color,
            ..Default::default()
        },
    );
    job
}

fn syntax_layout_job(
    text: &str,
    file_name: &str,
    dark: bool,
    font_id: &egui::FontId,
    fallback: Color32,
) -> egui::text::LayoutJob {
    use syntect::easy::HighlightLines;
    use syntect::highlighting::ThemeSet;
    use syntect::parsing::SyntaxSet;

    static SYNTAX: std::sync::OnceLock<(SyntaxSet, ThemeSet)> = std::sync::OnceLock::new();
    let (ps, ts) = SYNTAX.get_or_init(|| {
        (
            SyntaxSet::load_defaults_newlines(),
            ThemeSet::load_defaults(),
        )
    });
    let theme_name = if dark {
        "base16-ocean.dark"
    } else {
        "InspiredGitHub"
    };
    let theme = ts
        .themes
        .get(theme_name)
        .or_else(|| ts.themes.values().next());
    let mut job = egui::text::LayoutJob::default();
    let Some(theme) = theme else {
        return plain_layout_job(text, font_id, fallback);
    };
    let syntax = ps
        .find_syntax_for_file(file_name)
        .ok()
        .flatten()
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme);
    for line in syntect::util::LinesWithEndings::from(text) {
        let ranges = h.highlight_line(line, ps).unwrap_or_default();
        for (style, part) in ranges {
            let fg = style.foreground;
            job.append(
                part,
                0.0,
                egui::TextFormat {
                    font_id: font_id.clone(),
                    color: Color32::from_rgb(fg.r, fg.g, fg.b),
                    ..Default::default()
                },
            );
        }
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_count_handles_edge_cases() {
        assert_eq!(line_count(""), 1);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\nb"), 2);
        // 末尾换行后还有一个空行(编辑器里的光标行)
        assert_eq!(line_count("a\n"), 2);
        assert_eq!(line_count("a\n\n"), 3);
    }

    #[test]
    fn chunking_roundtrips_without_adding_boundary_newlines() {
        let text = (0..(EDITOR_CHUNK_LINES * 3 + 7))
            .map(|line| format!("第{line}行"))
            .collect::<Vec<_>>()
            .join("\n");
        let doc = EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), text.clone(), false);
        assert_eq!(doc.full_text(), text);
        assert_eq!(doc.line_count(), EDITOR_CHUNK_LINES * 3 + 7);
        assert!(
            doc.chunks
                .iter()
                .all(|chunk| chunk.lines <= EDITOR_CHUNK_LINES)
        );
    }

    #[test]
    fn chunking_preserves_empty_and_trailing_lines() {
        for text in ["", "\n", "a\n", "\na", "a\n\nb\n"] {
            let doc = EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), text.to_owned(), false);
            assert_eq!(doc.full_text(), text);
            assert_eq!(doc.line_count(), line_count(text));
        }
    }

    #[test]
    fn dirty_state_clears_when_content_returns_to_saved_baseline() {
        let mut doc = EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), "original".into(), false);

        doc.replace_full_text("changed".into());
        assert!(doc.dirty);
        doc.replace_full_text("original".into());
        assert!(!doc.dirty);

        doc.replace_full_text("OriginaL".into());
        assert!(doc.dirty, "等长但内容不同仍必须判定为脏");
    }

    #[test]
    fn save_completion_keeps_edits_made_after_save_started_dirty() {
        let mut doc = EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), "old".into(), false);
        doc.replace_full_text("snapshot".into());
        let saved_len = doc.text_len;
        let saved_digest = digest_chunks(&doc.chunks);

        doc.replace_full_text("newer edit".into());
        doc.mark_saved(saved_len, saved_digest);
        assert!(doc.dirty);

        doc.replace_full_text("snapshot".into());
        assert!(!doc.dirty);
    }

    #[test]
    fn close_all_clean_documents_normalizes_editor_state() {
        let mut editor = EditorState {
            show: true,
            docs: vec![
                EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), "a".into(), false),
                EditorDoc::new(1, "/tmp/b".into(), "b.txt".into(), "b".into(), false),
            ],
            active: 1,
            focus_search: true,
            ..Default::default()
        };

        assert!(editor.request_close_all());
        assert!(editor.docs.is_empty());
        assert_eq!(editor.active, 0);
        assert!(!editor.show);
        assert!(!editor.focus_search);
    }

    #[test]
    fn activating_document_requests_editor_viewport_focus_once() {
        let mut editor = EditorState {
            docs: vec![
                EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), "a".into(), false),
                EditorDoc::new(1, "/tmp/b".into(), "b.txt".into(), "b".into(), false),
            ],
            ..Default::default()
        };

        editor.activate_doc(1);
        assert_eq!(editor.active, 1);
        assert!(editor.show);
        assert!(std::mem::take(&mut editor.focus_viewport));
        assert!(!editor.focus_viewport);
    }

    #[test]
    fn opening_same_session_and_path_activates_existing_document() {
        let mut editor = EditorState {
            docs: vec![
                EditorDoc::new(1, "/etc/hosts".into(), "hosts".into(), "a".into(), false),
                EditorDoc::new(2, "/etc/hosts".into(), "hosts".into(), "b".into(), false),
            ],
            ..Default::default()
        };

        assert!(editor.activate_open_doc(2, "/etc/hosts"));
        assert_eq!(editor.active, 1);
        assert!(editor.show);
        assert!(editor.focus_viewport);
        assert!(!editor.activate_open_doc(1, "/etc/passwd"));
    }

    #[test]
    fn close_all_dirty_documents_defers_to_confirmation() {
        let mut dirty = EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), "changed".into(), false);
        dirty.dirty = true;
        let mut editor = EditorState {
            show: true,
            docs: vec![dirty],
            ..Default::default()
        };

        assert!(!editor.request_close_all());
        assert_eq!(editor.docs.len(), 1);
        assert!(editor.show);
        assert!(editor.confirm_close_all);
    }

    #[test]
    fn closing_clean_editor_respects_remember_open_files_setting() {
        let make_editor = || EditorState {
            show: true,
            docs: vec![EditorDoc::new(
                1,
                "/tmp/a".into(),
                "a.txt".into(),
                "a".into(),
                false,
            )],
            ..Default::default()
        };

        let mut forget = make_editor();
        forget.begin_clean_window_close(false);
        assert!(!forget.show);
        assert_eq!(forget.docs.len(), 1);
        forget.finish_viewport_frame();
        assert!(forget.docs.is_empty());

        let mut remember = make_editor();
        remember.begin_clean_window_close(true);
        assert!(!remember.show);
        remember.finish_viewport_frame();
        assert_eq!(remember.docs.len(), 1);
        assert_eq!(remember.docs[0].path, "/tmp/a");
    }

    #[test]
    fn editor_tab_uses_saved_display_name_then_host() {
        assert_eq!(
            ssh_editor_host_label("root@生产服务器", "root", "10.0.0.8"),
            "生产服务器"
        );
        assert_eq!(
            ssh_editor_host_label("root@example.com", "root", "example.com"),
            "example.com"
        );
        assert_eq!(
            ssh_editor_host_label("异常标题", "root", "10.0.0.8"),
            "10.0.0.8"
        );

        let mut doc = EditorDoc::new(1, "/etc/hosts".into(), "hosts".into(), String::new(), false);
        doc.host_label = "生产服务器".into();
        assert_eq!(editor_tab_label(&doc), "生产服务器 · hosts");
    }

    #[test]
    fn wrapped_gutter_numbers_only_logical_lines() {
        assert_eq!(
            wrapped_gutter_line_numbers(7, [false, false, true, true, false]),
            vec![Some(7), None, None, Some(8), Some(9)]
        );
    }

    #[test]
    fn wrap_estimate_counts_continuations_without_changing_logical_lines() {
        assert_eq!(estimated_visual_rows("12345678\n中中中", 4.0, 1.0), 4);
        assert_eq!(line_count("12345678\n中中中"), 2);
    }

    #[test]
    fn find_matches_case_sensitive() {
        let m = find_matches("Foo bar foo FOO", "foo", true);
        assert_eq!(m, vec![(8, 11)]);
        let m = find_matches("Foo bar foo FOO", "foo", false);
        assert_eq!(m, vec![(0, 3), (8, 11), (12, 15)]);
        assert!(find_matches("abc", "", true).is_empty());
    }

    #[test]
    fn find_matches_utf8_byte_ranges() {
        let m = find_matches("中文测试中文", "测试", true);
        assert_eq!(m, vec![(6, 12)]);
        // 字节区间必须落在字符边界上,replace_range 才不会 panic
        let mut text = String::from("中文测试中文");
        text.replace_range(m[0].0..m[0].1, "xx");
        assert_eq!(text, "中文xx中文");
    }
}
