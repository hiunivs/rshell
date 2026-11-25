// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! 在线预览/编辑:独立原生窗口(viewport)的文本编辑器——多文件标签、行号、
//! 语法高亮(syntect)、搜索/替换、保存回传、本地程序打开。
//! 类型判定层级(file_kind.rs):1 执行位 → 2 扩展名 → 4 前 8KB 内容嗅探 → 3 file 命令兜底。

use crate::app::RShellApp;
use crate::config::{EditorSettings, FileOpenMode};
use crate::file_kind::FileKind;
use crate::session::files::{FileEntry, SftpCmd, WriteFailureKind, WritePrivilege, WriteResult};
use crate::ui::sidebar::fmt_bytes;
use egui::{Color32, RichText, vec2};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use zeroize::{Zeroize, Zeroizing};

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
/// 拖选跨块时，合并后的块最多保留的行数。拖出视口时避免把整篇文件塞进一个
/// TextEdit；正常拖动通常只会合并一个相邻块。
const DRAG_MERGE_MAX_LINES: usize = EDITOR_CHUNK_LINES * 4;
/// 单帧最多连续合并的块数，快速拖过多个边界时也能跟上指针。
const DRAG_MERGE_PER_FRAME: usize = 4;
/// 搜索跳转后在命中行上方保留的可视行数，避免首行贴住并被顶部标签遮挡。
const SEARCH_JUMP_TOP_CONTEXT_ROWS: f32 = 2.0;

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
    /// 普通保存因目录权限失败后，显式请求 sudo 认证。
    privileged_save: Option<PrivilegedSavePrompt>,
    /// 远端内容发生变化时，必须由用户明确确认才允许覆盖。
    conflict_save: Option<ConflictSavePrompt>,
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
        if let Some(prompt) = self.privileged_save.as_mut() {
            prompt.password.zeroize();
        }
        self.docs.clear();
        self.active = 0;
        self.show = false;
        self.focus_viewport = false;
        self.confirm_close_all = false;
        self.focus_search = false;
        self.discard_docs_after_frame = false;
        self.privileged_save = None;
        self.conflict_save = None;
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
    /// 最近一次从磁盘读取或成功写回的原始字节摘要，用于防止覆盖外部修改。
    remote_digest: [u8; 32],
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
    /// 各分块末尾的 syntect 语法状态缓存(跨块高亮续接,见 HighlightChain)
    highlight: HighlightChain,
    /// 鼠标拖选跨块时的状态(见 extend_drag_selection)。
    drag_selection: Option<DragSelection>,
    /// 上一帧编辑区可视矩形(屏幕坐标)，用于把跨块合并限制在视口内。
    last_editor_viewport: Option<egui::Rect>,
}

/// 正在被鼠标拖选的块。
///
/// egui 的 TextEdit 选区无法跨控件，而编辑器为了控制排版量把文本拆成多个
/// TextEdit。拖到块边界后，把相邻块按需并入正在拖动的块，光标/选区就能
/// 继续越过原来的块边界，复制/删除也仍然作用在同一个 TextEdit 上。
#[derive(Clone, Copy)]
struct DragSelection {
    chunk_id: u64,
    /// 该块正文区域上一帧的屏幕矩形。
    rect: egui::Rect,
}

struct EditorChunk {
    id: u64,
    text: String,
    lines: usize,
    /// text 的 FNV-1a 摘要;语法状态链用它判断缓存是否仍有效(编辑点之后即失效)
    hash: u64,
    /// 自动换行模式下的已测量高度；只缓存数值，不保留 Galley。
    wrap_height: Option<WrapHeightCache>,
}

struct PendingSave {
    id: u64,
    len: usize,
    digest: [u8; 32],
    privileged: bool,
    force: bool,
}

struct PrivilegedSavePrompt {
    session_id: usize,
    path: String,
    password: String,
    error: Option<String>,
    force: bool,
    focus_password: bool,
}

impl Drop for PrivilegedSavePrompt {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

struct ConflictSavePrompt {
    session_id: usize,
    path: String,
    message: String,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SearchHighlightRange {
    start: usize,
    end: usize,
    current: bool,
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
            remote_digest: saved_digest,
            text_len,
            saving: None,
            close_after_save: false,
            error: None,
            lossy,
            confirm_close: false,
            search: SearchState::default(),
            line_count,
            scroll_to_line: None,
            highlight: HighlightChain::default(),
            drag_selection: None,
            last_editor_viewport: None,
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
        self.highlight.entries.clear();
        self.drag_selection = None;
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
        self.highlight.entries.clear();
        self.drag_selection = None;
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
                hash: text_hash(&current),
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
        hash: text_hash(&current),
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
        search.current = None;
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

/// 把文档级搜索字节区间映射为每个 TextEdit 分块内的局部区间。
///
/// 查询框是单行输入，因此正常匹配不会跨越分块边界；如果状态由测试或未来代码
/// 注入了包含换行的查询，跨块匹配会被安全跳过。
fn chunk_search_highlights(doc: &EditorDoc) -> Vec<Vec<SearchHighlightRange>> {
    let mut highlights = vec![Vec::new(); doc.chunks.len()];
    if !doc.search.open || doc.search.query.is_empty() {
        return highlights;
    }
    let Some(cache) = doc.search.matches_cache.as_ref().filter(|cache| {
        cache.text_gen == doc.text_gen
            && cache.query == doc.search.query
            && cache.case_sensitive == doc.search.case_sensitive
    }) else {
        return highlights;
    };

    let mut match_index = 0usize;
    let mut chunk_start = 0usize;
    for (chunk_index, chunk) in doc.chunks.iter().enumerate() {
        let chunk_end = chunk_start + chunk.text.len();
        while match_index < cache.matches.len() && cache.matches[match_index].1 <= chunk_start {
            match_index += 1;
        }
        while match_index < cache.matches.len() && cache.matches[match_index].0 < chunk_end {
            let (start, end) = cache.matches[match_index];
            if start >= chunk_start && end <= chunk_end {
                let local_start = start - chunk_start;
                let local_end = end - chunk_start;
                if chunk.text.is_char_boundary(local_start)
                    && chunk.text.is_char_boundary(local_end)
                {
                    highlights[chunk_index].push(SearchHighlightRange {
                        start: local_start,
                        end: local_end,
                        current: doc.search.current == Some(match_index),
                    });
                }
            }
            match_index += 1;
        }
        chunk_start = chunk_end + 1;
    }
    highlights
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
        // 编辑器可以同时打开多个会话的文件；保存结果必须从所有会话排空，不能
        // 依赖当前主标签，否则切换会话后保存状态会永久停在“保存中”。
        let writes: Vec<_> = self
            .sessions
            .iter_mut()
            .flat_map(|session| session.fm.write_inbox.drain(..))
            .collect();
        for (id, result) in writes {
            self.on_write_done(id, result);
        }
        let Some(idx) = self.effective_session_idx() else {
            return;
        };
        let (req_preview, req_external, content, kind, ext_done) = {
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
                fm.preview_inbox.pop_front(),
                fm.kind_inbox.pop_front(),
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
        let probe_contains_full_file = matches!(p.purpose, ReadPurpose::Probe)
            && probe_contains_complete_file(p.entry.size, data.len());
        match (p.purpose, sniffed) {
            (ReadPurpose::Probe, FileKind::Text) if probe_contains_full_file => {
                self.open_editor(&p.entry, p.path, data, false)
            }
            (ReadPurpose::Probe, FileKind::Text) => self.guarded_full_read(p.entry),
            (ReadPurpose::Probe, FileKind::Unknown) => {
                self.send_detect_kind(p.entry, p.path, probe_contains_full_file.then_some(data))
            }
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
        let remote_digest: [u8; 32] = Sha256::digest(&data).into();
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
        doc.remote_digest = remote_digest;
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

    fn on_write_done(&mut self, id: u64, result: WriteResult) {
        let Some(doc_idx) = self
            .editor
            .docs
            .iter()
            .position(|doc| doc.saving.as_ref().is_some_and(|saving| saving.id == id))
        else {
            return;
        };
        let saving = self.editor.docs[doc_idx]
            .saving
            .take()
            .expect("matching save checked");
        let session_id = self.editor.docs[doc_idx].session_id;
        let path = self.editor.docs[doc_idx].path.clone();
        let close_after = self.editor.docs[doc_idx].close_after_save;
        match result {
            Ok(()) => {
                let doc = &mut self.editor.docs[doc_idx];
                doc.mark_saved(saving.len, saving.digest);
                doc.remote_digest = saving.digest;
                doc.close_after_save = false;
            }
            Err(error) => {
                self.editor.docs[doc_idx].error = Some(format!("保存失败：{}", error.message));
                let is_ssh = self
                    .sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .is_some_and(|session| matches!(session.kind, crate::app::SessionKind::Ssh));
                match error.kind {
                    WriteFailureKind::PermissionDenied if !saving.privileged && is_ssh => {
                        self.editor.privileged_save = Some(PrivilegedSavePrompt {
                            session_id,
                            path,
                            password: String::new(),
                            error: Some(error.message),
                            force: saving.force,
                            focus_password: true,
                        });
                    }
                    WriteFailureKind::Authentication if saving.privileged => {
                        self.editor.privileged_save = Some(PrivilegedSavePrompt {
                            session_id,
                            path,
                            password: String::new(),
                            error: Some(error.message),
                            force: saving.force,
                            focus_password: true,
                        });
                    }
                    WriteFailureKind::Conflict => {
                        self.editor.conflict_save = Some(ConflictSavePrompt {
                            session_id,
                            path,
                            message: error.message,
                        });
                    }
                    _ => self.editor.docs[doc_idx].close_after_save = false,
                }
                return;
            }
        }
        if close_after {
            self.remove_editor_doc(doc_idx);
        }
    }

    fn save_editor_doc(&mut self, doc_idx: usize) {
        self.save_editor_doc_with(doc_idx, false, WritePrivilege::Normal);
    }

    fn save_editor_doc_with(&mut self, doc_idx: usize, force: bool, privilege: WritePrivilege) {
        let Some(doc) = self.editor.docs.get(doc_idx) else {
            return;
        };
        if doc.saving.is_some() {
            return;
        }
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
        let expected_sha256 = (!force).then_some(doc.remote_digest);
        let privileged = matches!(&privilege, WritePrivilege::Elevated { .. });
        let result = fm.send(SftpCmd::WriteFile {
            id,
            path: doc.path.clone(),
            data,
            expected_sha256,
            privilege,
        });
        match result {
            Ok(()) => {
                doc.saving = Some(PendingSave {
                    id,
                    len,
                    digest,
                    privileged,
                    force,
                });
                doc.error = None;
            }
            Err(e) => doc.error = Some(format!("保存失败:{e}")),
        }
    }

    fn show_save_resolution_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(mut prompt) = self.editor.privileged_save.take() {
            let doc_idx = self
                .editor
                .docs
                .iter()
                .position(|doc| doc.session_id == prompt.session_id && doc.path == prompt.path);
            let mut submit = false;
            let mut cancel = doc_idx.is_none();
            if let Some(doc_idx) = doc_idx {
                let file_name = self.editor.docs[doc_idx].file_name.clone();
                egui::Window::new("管理员保存")
                    .id(egui::Id::new("editor_privileged_save"))
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
                    .frame(crate::ui::dialog::modal_frame(ctx))
                    .show(ctx, |ui| {
                        ui.set_width(430.0);
                        ui.label(format!("当前 SSH 用户不能安全替换「{file_name}」。"));
                        ui.add(
                            egui::Label::new(
                                RichText::new(&prompt.path).monospace().small().weak(),
                            )
                            .wrap(),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            "RShell 将通过 sudo 校验原文件未被修改、保留所有者和权限，并原子替换。密码只发送到 sudo 的标准输入。",
                        );
                        if let Some(error) = &prompt.error {
                            ui.add_space(6.0);
                            ui.colored_label(Color32::from_rgb(0xD8, 0x3A, 0x3A), error);
                        }
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label("sudo 密码");
                            let response = ui.add_sized(
                                [300.0, 26.0],
                                egui::TextEdit::singleline(&mut prompt.password)
                                    .password(true)
                                    .hint_text("留空则使用免密 sudo 或已有凭据"),
                            );
                            if prompt.focus_password {
                                response.request_focus();
                                prompt.focus_password = false;
                            }
                        });
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if crate::ui::dialog::compact_primary_button(ui, "管理员保存").clicked()
                            {
                                submit = true;
                            }
                            if crate::ui::dialog::compact_secondary_button(ui, "取消").clicked() {
                                cancel = true;
                            }
                        });
                    });

                if submit {
                    let password = if prompt.password.is_empty() {
                        prompt.password.zeroize();
                        None
                    } else {
                        Some(Zeroizing::new(std::mem::take(&mut prompt.password)))
                    };
                    self.save_editor_doc_with(
                        doc_idx,
                        prompt.force,
                        WritePrivilege::Elevated { password },
                    );
                } else if cancel {
                    prompt.password.zeroize();
                    self.editor.docs[doc_idx].close_after_save = false;
                } else {
                    self.editor.privileged_save = Some(prompt);
                }
            } else {
                prompt.password.zeroize();
            }
        }

        if let Some(prompt) = self.editor.conflict_save.take() {
            let doc_idx = self
                .editor
                .docs
                .iter()
                .position(|doc| doc.session_id == prompt.session_id && doc.path == prompt.path);
            let mut overwrite = false;
            let mut cancel = doc_idx.is_none();
            if let Some(doc_idx) = doc_idx {
                let file_name = self.editor.docs[doc_idx].file_name.clone();
                egui::Window::new("文件已被修改")
                    .id(egui::Id::new("editor_save_conflict"))
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
                    .frame(crate::ui::dialog::modal_frame(ctx))
                    .show(ctx, |ui| {
                        ui.set_width(420.0);
                        ui.label(format!(
                            "「{file_name}」在打开后被其他程序修改。为避免丢失远端修改，RShell 没有覆盖它。"
                        ));
                        ui.add_space(6.0);
                        ui.label(RichText::new(&prompt.message).small().weak());
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if crate::ui::dialog::compact_destructive_button(ui, "仍然覆盖")
                                .clicked()
                            {
                                overwrite = true;
                            }
                            if crate::ui::dialog::compact_secondary_button(ui, "取消").clicked() {
                                cancel = true;
                            }
                        });
                    });
                if overwrite {
                    self.save_editor_doc_with(doc_idx, true, WritePrivilege::Normal);
                } else if cancel {
                    self.editor.docs[doc_idx].close_after_save = false;
                } else {
                    self.editor.conflict_save = Some(prompt);
                }
            }
        }
    }

    fn remove_editor_doc(&mut self, doc_idx: usize) {
        if let Some(doc) = self.editor.docs.get(doc_idx) {
            if self.editor.privileged_save.as_ref().is_some_and(|prompt| {
                prompt.session_id == doc.session_id && prompt.path == doc.path
            }) {
                if let Some(prompt) = self.editor.privileged_save.as_mut() {
                    prompt.password.zeroize();
                }
                self.editor.privileged_save = None;
            }
            if self.editor.conflict_save.as_ref().is_some_and(|prompt| {
                prompt.session_id == doc.session_id && prompt.path == doc.path
            }) {
                self.editor.conflict_save = None;
            }
        }
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
        if let Err(error) = crate::session::files::validate_sftp_name(&entry.name) {
            self.sessions[idx].fm.error = Some(format!("拒绝不安全的远端文件名: {error}"));
            return;
        }
        let session_id = self.sessions[idx].id;
        let mut nonce = [0u8; 8];
        rand::fill(&mut nonce);
        let dir = std::env::temp_dir()
            .join("rshell")
            .join(session_id.to_string())
            .join(format!("{:016x}", u64::from_ne_bytes(nonce)));
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
                    crate::ui::settings::apply_native_window_theme(&vp_ctx, self.config.theme_mode);
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
        let active_is_ssh = self
            .sessions
            .iter()
            .find(|session| session.id == self.editor.docs[active].session_id)
            .is_some_and(|session| matches!(session.kind, crate::app::SessionKind::Ssh));
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
                    if active_is_ssh && ui.button("以管理员权限保存…").clicked() {
                        ui.close();
                        let doc = &self.editor.docs[active];
                        self.editor.privileged_save = Some(PrivilegedSavePrompt {
                            session_id: doc.session_id,
                            path: doc.path.clone(),
                            password: String::new(),
                            error: None,
                            force: false,
                            focus_password: true,
                        });
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
            let (changed, needs_rebalance) = show_editor_document(
                ui,
                doc,
                &ext,
                &font_id,
                auto_wrap,
                jump_line,
                &self.config.editor,
            );
            if changed {
                doc.mark_text_changed();
            }
            if needs_rebalance {
                doc.rebalance();
            }
        });

        self.show_save_resolution_dialogs(ctx);

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
                        crate::ui::dialog::native_title_bar_height(),
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
                        crate::ui::dialog::native_title_bar_height(),
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

fn probe_contains_complete_file(entry_size: u64, bytes_read: usize) -> bool {
    (bytes_read as u64) < PROBE_BYTES || (entry_size > 0 && bytes_read as u64 >= entry_size)
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
    editor_settings: &EditorSettings,
) -> (bool, bool) {
    let row_height = ui.fonts_mut(|fonts| fonts.row_height(font_id));
    let available_width = ui.available_width();
    let digits = doc.line_count.max(1).ilog10() as usize + 1;
    let digit_width = ui.fonts_mut(|fonts| fonts.glyph_width(font_id, '0'));
    let gutter_width = (digits as f32 * digit_width + 12.0).ceil();
    let wrap_width = (available_width - gutter_width - ui.spacing().item_spacing.x).max(24.0);
    let width_bits = wrap_width.to_bits();
    let font_size_bits = font_id.size.to_bits();
    // 拖选越过块边界时先合并相邻块，再做搜索高亮/语法状态计算，
    // 保证后续分块下标与本帧实际绘制一致。
    extend_drag_selection(
        ui.ctx(),
        doc,
        row_height,
        auto_wrap,
        wrap_width,
        width_bits,
        font_size_bits,
        digit_width,
    );
    let search_highlights = chunk_search_highlights(doc);
    let env = HighlightEnv::new(file_name, ui.visuals().dark_mode, editor_settings);

    let jump_offset = jump_line.map(|line| {
        let line_offset = if auto_wrap {
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
        };
        search_jump_scroll_offset(line_offset, row_height)
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
        doc.last_editor_viewport = Some(ui.clip_rect());
        if auto_wrap {
            show_wrapped_chunks(
                ui,
                viewport,
                doc,
                &env,
                font_id,
                row_height,
                gutter_width,
                wrap_width,
                width_bits,
                font_size_bits,
                digit_width,
                &search_highlights,
                &mut changed,
                &mut needs_rebalance,
            );
        } else {
            show_unwrapped_chunks(
                ui,
                viewport,
                doc,
                &env,
                font_id,
                row_height,
                gutter_width,
                &search_highlights,
                &mut changed,
                &mut needs_rebalance,
            );
        }
    });
    (changed, needs_rebalance)
}

fn search_jump_scroll_offset(line_offset: f32, row_height: f32) -> f32 {
    (line_offset - row_height * SEARCH_JUMP_TOP_CONTEXT_ROWS).max(0.0)
}

#[allow(clippy::too_many_arguments)]
fn show_unwrapped_chunks(
    ui: &mut egui::Ui,
    viewport: egui::Rect,
    doc: &mut EditorDoc,
    env: &HighlightEnv,
    font_id: &egui::FontId,
    row_height: f32,
    gutter_width: f32,
    search_highlights: &[Vec<SearchHighlightRange>],
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
    // 语法状态沿块序推进:可见块从上一块的末尾状态续接高亮;不可见块只在
    // 缓存失效时做一次纯解析(不排版),窗口之后的块等滚动到再算。
    let highlight = &mut doc.highlight;
    let mut chain = env.theme_key;
    let mut states: Option<HighlightStates> = None;
    let mut first_line = 0usize;
    let drag_chunk_id = doc.drag_selection.map(|drag| drag.chunk_id);
    let mut drag_started: Option<(u64, egui::Rect)> = None;
    let mut drag_stopped = false;
    let mut drag_rect: Option<egui::Rect> = None;
    for (chunk_index, chunk) in doc.chunks.iter_mut().enumerate() {
        let actual_lines = chunk.lines;
        let chunk_top = first_line as f32 * row_height;
        let chunk_bottom = chunk_top + actual_lines as f32 * row_height;
        if chunk_bottom < visible_top || chunk_top > visible_bottom {
            if !env.plain && chunk_top <= visible_bottom {
                chain = chain_mix(chain, chunk.hash);
                states = match highlight.entries.get(&chunk.id) {
                    Some(entry) if entry.chain == chain => {
                        Some((entry.parse.clone(), entry.highlight.clone()))
                    }
                    _ => {
                        let start = states.unwrap_or_else(|| fresh_highlight_states(env));
                        let end = advance_highlight_states(env, &chunk.text, start);
                        highlight.entries.insert(
                            chunk.id,
                            ChainEntry {
                                chain,
                                parse: end.0.clone(),
                                highlight: end.1.clone(),
                            },
                        );
                        Some(end)
                    }
                };
            }
            first_line += actual_lines;
            continue;
        }
        chain = chain_mix(chain, chunk.hash);
        // 块内编辑不影响块首状态,上一块算出的末尾状态即为正确的续接起点
        let start_states = if env.plain {
            None
        } else {
            Some(
                states
                    .clone()
                    .unwrap_or_else(|| fresh_highlight_states(env)),
            )
        };
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
        let initial_text = chunk.text.clone();
        let (initial_galley, end_states) = syntax_layout(
            ui,
            &initial_text,
            env,
            start_states.clone(),
            font_id,
            f32::INFINITY,
            &search_highlights[chunk_index],
        );
        if let Some(end) = end_states {
            highlight.entries.insert(
                chunk.id,
                ChainEntry {
                    chain,
                    parse: end.0.clone(),
                    highlight: end.1.clone(),
                },
            );
            states = Some(end);
        }
        ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt(("editor_chunk_ui", chunk.id))
                .max_rect(text_rect)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
            |ui| {
                ui.set_clip_rect(text_clip);
                let cached_galley = initial_galley.clone();
                let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
                    if buf.as_str() == initial_text {
                        cached_galley.clone()
                    } else {
                        // 块内编辑不改变块首语法状态,沿用同一续接起点即可
                        syntax_layout(
                            ui,
                            buf.as_str(),
                            env,
                            start_states.clone(),
                            font_id,
                            f32::INFINITY,
                            &search_highlights[chunk_index],
                        )
                        .0
                    }
                };
                // 拖选合并后的块可能超过常规块大小；下限仍按单块上限，
                // 允许继续输入，但不会无限增长（失去焦点后会重新分块）。
                let char_limit = EDITOR_CHUNK_BYTES.max(chunk.text.chars().count());
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut chunk.text)
                        .id(editor_id)
                        .font(font_id.clone())
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .desired_rows(actual_lines)
                        .char_limit(char_limit)
                        .layouter(&mut layouter),
                );
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    drag_started = Some((chunk.id, text_rect));
                }
                if resp.drag_stopped_by(egui::PointerButton::Primary) {
                    drag_stopped = true;
                }
                if drag_chunk_id == Some(chunk.id) {
                    drag_rect = Some(text_rect);
                }
                if resp.changed() {
                    chunk.lines = line_count(&chunk.text);
                    chunk.hash = text_hash(&chunk.text);
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
    finish_drag_tracking(doc, drag_chunk_id, drag_started, drag_stopped, drag_rect);
}

#[allow(clippy::too_many_arguments)]
fn show_wrapped_chunks(
    ui: &mut egui::Ui,
    viewport: egui::Rect,
    doc: &mut EditorDoc,
    env: &HighlightEnv,
    font_id: &egui::FontId,
    row_height: f32,
    gutter_width: f32,
    wrap_width: f32,
    width_bits: u32,
    font_size_bits: u32,
    digit_width: f32,
    search_highlights: &[Vec<SearchHighlightRange>],
    changed: &mut bool,
    needs_rebalance: &mut bool,
) {
    ui.spacing_mut().item_spacing.y = 0.0;
    let content_width = ui.available_width();
    let overscan = row_height * 12.0;
    let visible_top = (viewport.top() - overscan).max(0.0);
    let visible_bottom = viewport.bottom() + overscan;
    // 语法状态推进逻辑与 show_unwrapped_chunks 相同:窗口前填充,窗口后惰性。
    let highlight = &mut doc.highlight;
    let mut chain = env.theme_key;
    let mut states: Option<HighlightStates> = None;
    let mut first_line = 0usize;
    let mut chunk_top = 0.0;
    let drag_chunk_id = doc.drag_selection.map(|drag| drag.chunk_id);
    let mut drag_started: Option<(u64, egui::Rect)> = None;
    let mut drag_stopped = false;
    let mut drag_rect: Option<egui::Rect> = None;

    for (chunk_index, chunk) in doc.chunks.iter_mut().enumerate() {
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
            if !env.plain && chunk_top <= visible_bottom {
                chain = chain_mix(chain, chunk.hash);
                states = match highlight.entries.get(&chunk.id) {
                    Some(entry) if entry.chain == chain => {
                        Some((entry.parse.clone(), entry.highlight.clone()))
                    }
                    _ => {
                        let start = states.unwrap_or_else(|| fresh_highlight_states(env));
                        let end = advance_highlight_states(env, &chunk.text, start);
                        highlight.entries.insert(
                            chunk.id,
                            ChainEntry {
                                chain,
                                parse: end.0.clone(),
                                highlight: end.1.clone(),
                            },
                        );
                        Some(end)
                    }
                };
            }
            ui.allocate_space(vec2(content_width, estimated_height));
            chunk_top += estimated_height;
            first_line += chunk.lines;
            continue;
        }

        chain = chain_mix(chain, chunk.hash);
        // 块内编辑不影响块首状态,上一块算出的末尾状态即为正确的续接起点
        let start_states = if env.plain {
            None
        } else {
            Some(
                states
                    .clone()
                    .unwrap_or_else(|| fresh_highlight_states(env)),
            )
        };
        let initial_text = chunk.text.clone();
        let (initial_galley, end_states) = syntax_layout(
            ui,
            &initial_text,
            env,
            start_states.clone(),
            font_id,
            wrap_width,
            &search_highlights[chunk_index],
        );
        if let Some(end) = end_states {
            highlight.entries.insert(
                chunk.id,
                ChainEntry {
                    chain,
                    parse: end.0.clone(),
                    highlight: end.1.clone(),
                },
            );
            states = Some(end);
        }
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
                        // 块内编辑不改变块首语法状态,沿用同一续接起点即可
                        syntax_layout(
                            ui,
                            buf.as_str(),
                            env,
                            start_states.clone(),
                            font_id,
                            requested_width,
                            &search_highlights[chunk_index],
                        )
                        .0
                    }
                };
                let char_limit = EDITOR_CHUNK_BYTES.max(chunk.text.chars().count());
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut chunk.text)
                        .id(editor_id)
                        .font(font_id.clone())
                        .frame(egui::Frame::NONE)
                        .desired_width(text_rect.width())
                        .desired_rows(1)
                        .char_limit(char_limit)
                        .layouter(&mut layouter),
                );
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    drag_started = Some((chunk.id, text_rect));
                }
                if resp.drag_stopped_by(egui::PointerButton::Primary) {
                    drag_stopped = true;
                }
                if drag_chunk_id == Some(chunk.id) {
                    drag_rect = Some(text_rect);
                }
                if resp.changed() {
                    chunk.lines = line_count(&chunk.text);
                    chunk.hash = text_hash(&chunk.text);
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
    finish_drag_tracking(doc, drag_chunk_id, drag_started, drag_stopped, drag_rect);
}

/// 根据本帧各块 TextEdit 的响应更新拖选状态。
fn finish_drag_tracking(
    doc: &mut EditorDoc,
    drag_chunk_id: Option<u64>,
    drag_started: Option<(u64, egui::Rect)>,
    drag_stopped: bool,
    drag_rect: Option<egui::Rect>,
) {
    if drag_stopped {
        doc.drag_selection = None;
        return;
    }
    if let Some((chunk_id, rect)) = drag_started {
        doc.drag_selection = Some(DragSelection { chunk_id, rect });
        return;
    }
    if let (Some(selection), Some(rect)) = (doc.drag_selection.as_mut(), drag_rect)
        && Some(selection.chunk_id) == drag_chunk_id
    {
        selection.rect = rect;
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
            current.hash = text_hash(&current.text);
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
            current.hash = text_hash(&current.text);
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

fn chunk_text_edit_id(chunk_id: u64) -> egui::Id {
    egui::Id::new(("editor_chunk", chunk_id))
}

/// 合并相邻块后，正在编辑的块文本整体前移了 `char_offset` 个字符：
/// 光标与选区锚点都要同步平移，否则选区会跳到错误位置。
/// 同时重置撤销历史——旧快照只包含合并前的块，撤销会丢掉刚并入的行。
fn shift_chunk_cursor_and_reset_undo(ctx: &egui::Context, chunk_id: u64, char_offset: usize) {
    let id = chunk_text_edit_id(chunk_id);
    let Some(mut state) = egui::text_edit::TextEditState::load(ctx, id) else {
        return;
    };
    if let Some(mut range) = state.cursor.char_range() {
        range.primary += char_offset;
        range.secondary += char_offset;
        state.cursor.set_char_range(Some(range));
    }
    state.set_undoer(egui::util::undoer::Undoer::with_settings(
        egui::util::undoer::Settings {
            max_undos: EDITOR_UNDO_LIMIT,
            ..Default::default()
        },
    ));
    state.store(ctx, id);
}

/// 块被并入相邻块后它的 TextEdit 不再存在，清掉旧状态，避免 id 复用时串用光标。
fn forget_chunk_state(ctx: &egui::Context, chunk_id: u64) {
    ctx.data_mut(|data| {
        data.remove::<egui::text_edit::TextEditState>(chunk_text_edit_id(chunk_id))
    });
}

fn chunk_display_height(
    chunk: &EditorChunk,
    row_height: f32,
    auto_wrap: bool,
    wrap_width: f32,
    width_bits: u32,
    font_size_bits: u32,
    digit_width: f32,
) -> f32 {
    if auto_wrap {
        cached_or_estimated_wrap_height(
            chunk,
            wrap_width,
            row_height,
            width_bits,
            font_size_bits,
            digit_width,
        )
    } else {
        chunk.lines as f32 * row_height
    }
}

/// 把 `idx-1` 块并入 `idx` 块（保留 `idx` 块的 TextEdit id 和焦点）。
fn merge_previous_chunk(ctx: &egui::Context, doc: &mut EditorDoc, idx: usize) -> bool {
    if idx == 0 || idx >= doc.chunks.len() {
        return false;
    }
    let previous = doc.chunks.remove(idx - 1);
    let prefix_chars = previous.text.chars().count() + 1;
    {
        // 移除前一块后，当前块的下标前移一位。
        let current = &mut doc.chunks[idx - 1];
        current.text = format!("{}\n{}", previous.text, current.text);
        current.lines += previous.lines;
        current.hash = text_hash(&current.text);
        current.wrap_height = None;
    }
    doc.highlight.entries.remove(&previous.id);
    shift_chunk_cursor_and_reset_undo(ctx, doc.chunks[idx - 1].id, prefix_chars);
    forget_chunk_state(ctx, previous.id);
    doc.recount();
    true
}

/// 把 `idx+1` 块并入 `idx` 块；光标在合并前的位置不变。
fn merge_next_chunk(ctx: &egui::Context, doc: &mut EditorDoc, idx: usize) -> bool {
    if idx + 1 >= doc.chunks.len() {
        return false;
    }
    let next = doc.chunks.remove(idx + 1);
    {
        let current = &mut doc.chunks[idx];
        current.text.push('\n');
        current.text.push_str(&next.text);
        current.lines += next.lines;
        current.hash = text_hash(&current.text);
        current.wrap_height = None;
    }
    doc.highlight.entries.remove(&next.id);
    forget_chunk_state(ctx, next.id);
    doc.recount();
    true
}

/// 鼠标拖选越过块边界时，把相邻块按需并入正在拖动的块。
///
/// egui 的 TextEdit 选区不能跨控件，而编辑器按 96 行分块排版；不合并时，
/// 反向拖选会在块首（正向拖选在块尾）停住。合并只改变分块方式，全文内容
/// 不变，因此复制/删除/撤销仍由同一个 TextEdit 完成。指针离开编辑区视口
/// 后停止合并，避免一次拖出界把整篇文件塞进一个 TextEdit。
#[allow(clippy::too_many_arguments)]
fn extend_drag_selection(
    ctx: &egui::Context,
    doc: &mut EditorDoc,
    row_height: f32,
    auto_wrap: bool,
    wrap_width: f32,
    width_bits: u32,
    font_size_bits: u32,
    digit_width: f32,
) {
    let Some(drag) = doc.drag_selection else {
        return;
    };
    if !ctx.input(|i| i.pointer.primary_down()) {
        doc.drag_selection = None;
        return;
    }
    if ctx.memory(|m| m.focused()) != Some(chunk_text_edit_id(drag.chunk_id)) {
        doc.drag_selection = None;
        return;
    }
    let Some(pointer) = ctx.input(|i| i.pointer.interact_pos()) else {
        return;
    };
    if let Some(viewport) = doc.last_editor_viewport
        && (pointer.y < viewport.top() || pointer.y > viewport.bottom())
    {
        return;
    }

    let mut rect = drag.rect;
    for _ in 0..DRAG_MERGE_PER_FRAME {
        let Some(idx) = doc
            .chunks
            .iter()
            .position(|chunk| chunk.id == drag.chunk_id)
        else {
            doc.drag_selection = None;
            return;
        };
        if pointer.y < rect.top() && idx > 0 {
            let merged_lines = doc.chunks[idx - 1].lines + doc.chunks[idx].lines;
            if merged_lines > DRAG_MERGE_MAX_LINES {
                break;
            }
            let height = chunk_display_height(
                &doc.chunks[idx - 1],
                row_height,
                auto_wrap,
                wrap_width,
                width_bits,
                font_size_bits,
                digit_width,
            );
            if !merge_previous_chunk(ctx, doc, idx) {
                break;
            }
            // 合并后该块从上一块的顶部开始，底边不动。
            rect.min.y -= height;
        } else if pointer.y > rect.bottom() && idx + 1 < doc.chunks.len() {
            let merged_lines = doc.chunks[idx].lines + doc.chunks[idx + 1].lines;
            if merged_lines > DRAG_MERGE_MAX_LINES {
                break;
            }
            let height = chunk_display_height(
                &doc.chunks[idx + 1],
                row_height,
                auto_wrap,
                wrap_width,
                width_bits,
                font_size_bits,
                digit_width,
            );
            if !merge_next_chunk(ctx, doc, idx) {
                break;
            }
            rect.max.y += height;
        } else {
            break;
        }
    }
    doc.drag_selection = Some(DragSelection {
        chunk_id: drag.chunk_id,
        rect,
    });
}

/// 对单块排版(可能从指定语法状态续接);返回 Galley 与块末状态。
/// `start_states` 为 None 时按纯文本排版(plain 语法或无高亮环境)。
#[allow(clippy::too_many_arguments)]
fn syntax_layout(
    ui: &egui::Ui,
    text: &str,
    env: &HighlightEnv,
    start_states: Option<HighlightStates>,
    font_id: &egui::FontId,
    wrap_width: f32,
    search_highlights: &[SearchHighlightRange],
) -> (std::sync::Arc<egui::Galley>, Option<HighlightStates>) {
    let color = ui.visuals().text_color();
    let (mut job, end_states) = match start_states {
        Some(states) => highlight_chunk_layout(env, text, states, font_id, color),
        None => (plain_layout_job(text, font_id, color), None),
    };
    apply_search_highlights(&mut job, text, search_highlights, env.dark);
    job.wrap.max_width = wrap_width;
    job.wrap.break_anywhere = wrap_width.is_finite();
    job.keep_trailing_whitespace = true;
    (ui.fonts_mut(|f| f.layout_job(job)), end_states)
}

fn apply_search_highlights(
    job: &mut egui::text::LayoutJob,
    text: &str,
    highlights: &[SearchHighlightRange],
    dark: bool,
) {
    let mut valid = highlights
        .iter()
        .copied()
        .filter(|highlight| {
            highlight.start < highlight.end
                && highlight.end <= text.len()
                && text.is_char_boundary(highlight.start)
                && text.is_char_boundary(highlight.end)
        })
        .collect::<Vec<_>>();
    valid.sort_unstable_by_key(|highlight| (highlight.start, highlight.end));
    if valid.is_empty() {
        return;
    }

    let passive_background = if dark {
        Color32::from_rgb(0x5C, 0x4E, 0x1E)
    } else {
        Color32::from_rgb(0xFF, 0xEB, 0x96)
    };
    let current_background = if dark {
        Color32::from_rgb(0xA0, 0x68, 0x18)
    } else {
        Color32::from_rgb(0xFF, 0xB7, 0x4D)
    };
    let current_underline = if dark {
        Color32::from_rgb(0xFF, 0xD5, 0x66)
    } else {
        Color32::from_rgb(0x9C, 0x57, 0x00)
    };

    let original_sections = std::mem::take(&mut job.sections);
    let mut sections = Vec::with_capacity(original_sections.len() + valid.len() * 2);
    let mut next_highlight = 0usize;
    for section in original_sections {
        let section_start = section.byte_range.start.0;
        let section_end = section.byte_range.end.0;
        if section_start == section_end {
            sections.push(section);
            continue;
        }

        let mut cursor = section_start;
        let mut first_part = true;
        while next_highlight < valid.len() && valid[next_highlight].end <= section_start {
            next_highlight += 1;
        }
        let mut highlight_index = next_highlight;
        while highlight_index < valid.len() && valid[highlight_index].start < section_end {
            let highlight = &valid[highlight_index];
            let highlight_start = highlight.start.max(section_start).max(cursor);
            let highlight_end = highlight.end.min(section_end);
            if cursor < highlight_start {
                let mut part = section.clone();
                part.byte_range = cursor.into()..highlight_start.into();
                part.leading_space = if first_part {
                    section.leading_space
                } else {
                    0.0
                };
                sections.push(part);
                first_part = false;
            }
            if highlight_start < highlight_end {
                let mut part = section.clone();
                part.byte_range = highlight_start.into()..highlight_end.into();
                part.leading_space = if first_part {
                    section.leading_space
                } else {
                    0.0
                };
                part.format.background = if highlight.current {
                    current_background
                } else {
                    passive_background
                };
                if highlight.current {
                    part.format.underline = egui::Stroke::new(1.5, current_underline);
                }
                sections.push(part);
                first_part = false;
                cursor = highlight_end;
            }
            if highlight.end <= section_end {
                highlight_index += 1;
                next_highlight = highlight_index;
            } else {
                break;
            }
        }
        if cursor < section_end {
            let mut part = section;
            part.byte_range = cursor.into()..section_end.into();
            part.leading_space = if first_part { part.leading_space } else { 0.0 };
            sections.push(part);
        }
    }
    job.sections = sections;
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

// ---------------------------------------------------------------------------
// 语法高亮资源(内建 + 配置目录自定义)与跨块状态续接
// ---------------------------------------------------------------------------

/// 全局只加载一次:内建语法/主题,加上配置目录下用户放置的
/// `syntaxes/*.sublime-syntax` 与 `themes/*.tmTheme`。
struct HighlightAssets {
    syntax_set: SyntaxSet,
    themes: ThemeSet,
}

fn highlight_assets() -> &'static HighlightAssets {
    static ASSETS: std::sync::OnceLock<HighlightAssets> = std::sync::OnceLock::new();
    ASSETS.get_or_init(|| {
        let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
        // lines_include_newline=true,与 load_defaults_newlines 保持一致
        if let Some(dir) = crate::config::config_dir().map(|dir| dir.join("syntaxes"))
            && let Err(err) = builder.add_from_folder(&dir, true)
        {
            eprintln!("加载自定义语法目录 {} 失败: {err}", dir.display());
        }
        let mut themes = ThemeSet::load_defaults();
        if let Some(dir) = crate::config::config_dir().map(|dir| dir.join("themes"))
            && let Err(err) = themes.add_from_folder(&dir)
        {
            eprintln!("加载自定义主题目录 {} 失败: {err}", dir.display());
        }
        HighlightAssets {
            syntax_set: builder.build(),
            themes,
        }
    })
}

/// 设置页可选的高亮主题(内建 + 自定义 .tmTheme 的文件名)
pub(crate) fn available_highlight_themes() -> Vec<String> {
    let mut names: Vec<String> = highlight_assets().themes.themes.keys().cloned().collect();
    names.sort();
    names
}

/// 解析/高亮推进到某一块末尾时的 syntect 状态,下一块可从中续接而不必从头再解析。
type HighlightStates = (ParseState, HighlightState);

/// 解析完某一块后的状态快照。chain 是从文档起点到该块(含)的文本哈希链式折叠,
/// 任何前置块编辑、分块重组或主题切换都会使其失配,从而按需重算。
struct ChainEntry {
    chain: u64,
    parse: ParseState,
    highlight: HighlightState,
}

/// 每文档一份的分块语法状态缓存(按 chunk.id 索引;整块重建时清空)
#[derive(Default)]
struct HighlightChain {
    entries: std::collections::HashMap<u64, ChainEntry>,
}

/// 每帧解析一次的高亮环境:语法(按文件名)+主题(按亮暗模式与配置)
#[derive(Clone, Copy)]
struct HighlightEnv {
    syntax_set: &'static SyntaxSet,
    syntax: &'static SyntaxReference,
    theme: &'static Theme,
    /// 主题默认背景色;样式背景与它相同时不额外画底色
    theme_bg: Option<Color32>,
    dark: bool,
    /// 混入分块链式哈希,主题/亮暗切换后状态缓存自动失效
    theme_key: u64,
    /// 纯文本语法:跳过高亮与状态跟踪,直接单色排版
    plain: bool,
}

impl HighlightEnv {
    fn new(file_name: &str, dark: bool, settings: &EditorSettings) -> Self {
        let assets = highlight_assets();
        let syntax = assets
            .syntax_set
            .find_syntax_for_file(file_name)
            .ok()
            .flatten()
            .unwrap_or_else(|| assets.syntax_set.find_syntax_plain_text());
        let plain = std::ptr::eq(syntax, assets.syntax_set.find_syntax_plain_text());
        let configured = if dark {
            settings.highlight_theme_dark.as_str()
        } else {
            settings.highlight_theme_light.as_str()
        };
        let fallback_name = if dark {
            "base16-ocean.dark"
        } else {
            "InspiredGitHub"
        };
        let name = if configured.is_empty() {
            fallback_name
        } else {
            configured
        };
        let (theme, resolved) = assets
            .themes
            .themes
            .get(name)
            .map(|theme| (theme, name))
            .or_else(|| {
                assets
                    .themes
                    .themes
                    .get(fallback_name)
                    .map(|theme| (theme, fallback_name))
            })
            .or_else(|| {
                assets
                    .themes
                    .themes
                    .values()
                    .next()
                    .map(|theme| (theme, name))
            })
            .expect("syntect 内建主题集非空");
        Self {
            syntax_set: &assets.syntax_set,
            syntax,
            theme,
            theme_bg: theme
                .settings
                .background
                .map(|c| Color32::from_rgb(c.r, c.g, c.b)),
            dark,
            theme_key: chain_mix(text_hash(resolved), u64::from(dark)),
            plain,
        }
    }
}

/// FNV-1a;只用于进程内分块缓存的有效性判断,不持久化
fn text_hash(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn chain_mix(acc: u64, value: u64) -> u64 {
    let mut mixed = acc ^ value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    mixed = mixed.wrapping_mul(0xa24b_aed4_963e_e407);
    mixed ^ (mixed >> 32)
}

fn fresh_highlight_states(env: &HighlightEnv) -> HighlightStates {
    let highlighter = Highlighter::new(env.theme);
    (
        ParseState::new(env.syntax),
        HighlightState::new(&highlighter, ScopeStack::new()),
    )
}

/// 只推进语法状态不排版(不可见块与缓存失效间隙的填充)
fn advance_highlight_states(
    env: &HighlightEnv,
    text: &str,
    states: HighlightStates,
) -> HighlightStates {
    let (mut parse, mut highlight) = states;
    let highlighter = Highlighter::new(env.theme);
    for line in LinesWithEndings::from(text) {
        let Ok(ops) = parse.parse_line(line, env.syntax_set) else {
            continue;
        };
        // HighlightIterator 在迭代过程中推进 highlight 状态
        for _ in HighlightIterator::new(&mut highlight, &ops[..], line, &highlighter) {}
    }
    (parse, highlight)
}

/// 从给定状态续接,对单块做语法高亮排版;返回 LayoutJob 与块末状态。
/// 超大块退化为纯文本排版但仍推进状态,后续块的高亮才不会错乱。
fn highlight_chunk_layout(
    env: &HighlightEnv,
    text: &str,
    states: HighlightStates,
    font_id: &egui::FontId,
    fallback: Color32,
) -> (egui::text::LayoutJob, Option<HighlightStates>) {
    if text.len() > HIGHLIGHT_MAX_BYTES {
        let end = advance_highlight_states(env, text, states);
        return (plain_layout_job(text, font_id, fallback), Some(end));
    }
    let (mut parse, mut highlight) = states;
    let highlighter = Highlighter::new(env.theme);
    let mut job = egui::text::LayoutJob::default();
    for line in LinesWithEndings::from(text) {
        let Ok(ops) = parse.parse_line(line, env.syntax_set) else {
            job.append(
                line,
                0.0,
                egui::TextFormat {
                    font_id: font_id.clone(),
                    color: fallback,
                    ..Default::default()
                },
            );
            continue;
        };
        for (style, part) in HighlightIterator::new(&mut highlight, &ops[..], line, &highlighter) {
            let fg = style.foreground;
            let bg = Color32::from_rgb(style.background.r, style.background.g, style.background.b);
            job.append(
                part,
                0.0,
                egui::TextFormat {
                    font_id: font_id.clone(),
                    color: Color32::from_rgb(fg.r, fg.g, fg.b),
                    // 与编辑器底色相同的背景不重复绘制,避免全文被色块覆盖
                    background: if env.theme_bg == Some(bg) {
                        Color32::TRANSPARENT
                    } else {
                        bg
                    },
                    italics: style.font_style.contains(FontStyle::ITALIC),
                    ..Default::default()
                },
            );
        }
    }
    (job, Some((parse, highlight)))
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
    fn merging_previous_chunk_keeps_text_and_shifts_selection() {
        let ctx = egui::Context::default();
        let text = (0..(EDITOR_CHUNK_LINES + 4))
            .map(|line| format!("第{line}行"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut doc = EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), text.clone(), false);
        assert_eq!(doc.chunks.len(), 2);
        let focused = doc.chunks[1].id;
        let previous_chars = doc.chunks[0].text.chars().count();
        let previous_lines = doc.chunks[0].lines;
        let editor_id = chunk_text_edit_id(focused);
        let mut state = egui::text_edit::TextEditState::default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(10),
            )));
        state.store(&ctx, editor_id);

        assert!(merge_previous_chunk(&ctx, &mut doc, 1));
        assert_eq!(doc.chunks.len(), 1);
        assert_eq!(doc.chunks[0].id, focused);
        assert_eq!(doc.chunks[0].lines, previous_lines + 4);
        assert_eq!(doc.full_text(), text, "合并只改变分块,不能改动全文");

        let state = egui::text_edit::TextEditState::load(&ctx, editor_id).expect("state kept");
        let range = state.cursor.char_range().expect("selection kept");
        assert_eq!(range.secondary.index.0, previous_chars + 1);
        assert_eq!(range.primary.index.0, previous_chars + 11);
    }

    #[test]
    fn merging_next_chunk_keeps_text_and_selection() {
        let ctx = egui::Context::default();
        let text = (0..(EDITOR_CHUNK_LINES * 2 + 3))
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut doc = EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), text.clone(), false);
        assert_eq!(doc.chunks.len(), 3);
        let focused = doc.chunks[0].id;
        let editor_id = chunk_text_edit_id(focused);
        let mut state = egui::text_edit::TextEditState::default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(5),
                egui::text::CCursor::new(8),
            )));
        state.store(&ctx, editor_id);

        assert!(merge_next_chunk(&ctx, &mut doc, 0));
        assert_eq!(doc.chunks.len(), 2);
        assert_eq!(doc.chunks[0].id, focused);
        assert_eq!(doc.full_text(), text);

        let state = egui::text_edit::TextEditState::load(&ctx, editor_id).expect("state kept");
        let range = state.cursor.char_range().expect("selection kept");
        assert_eq!(range.secondary.index.0, 5);
        assert_eq!(range.primary.index.0, 8);
    }

    #[test]
    fn dragging_exactly_at_chunk_boundary_merges_previous_chunk() {
        let ctx = egui::Context::default();
        let text = (0..(EDITOR_CHUNK_LINES * 2 + 3))
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut doc = EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), text.clone(), false);
        let focused = doc.chunks[1].id;
        let editor_id = chunk_text_edit_id(focused);

        let row_height = 16.0;
        let rect = egui::Rect::from_min_size(
            egui::pos2(10.0, 300.0),
            egui::vec2(400.0, doc.chunks[1].lines as f32 * row_height),
        );
        doc.last_editor_viewport = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 600.0),
        ));
        doc.drag_selection = Some(DragSelection {
            chunk_id: focused,
            rect,
        });

        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::PointerButton {
            pos: egui::pos2(20.0, rect.top() - 1.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run_ui(input, |ui| {
            ui.memory_mut(|memory| memory.request_focus(editor_id));
            extend_drag_selection(ui.ctx(), &mut doc, row_height, false, 400.0, 0, 0, 8.0);
        });

        assert_eq!(doc.chunks.len(), 2, "向上越过块边界应把上一块并入");
        assert_eq!(doc.chunks[0].id, focused);
        assert_eq!(doc.full_text(), text);
        let drag = doc.drag_selection.expect("drag continues");
        assert!(drag.rect.top() < rect.top(), "合并后拖动块的矩形应向上扩展");
        assert_eq!(drag.rect.bottom(), rect.bottom());
    }

    #[test]
    fn dragging_editor_selection_across_chunk_boundary_selects_both_lines() {
        let text = (0..(EDITOR_CHUNK_LINES * 2))
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let doc = EditorDoc::new(1, "/tmp/a".into(), "a.txt".into(), text.clone(), false);
        assert_eq!(doc.chunks.len(), 2);
        let focused_id = doc.chunks[1].id;
        let font = egui::FontId::monospace(13.0);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 400.0))
            .build_ui_state(
                move |ui, doc: &mut EditorDoc| {
                    let _ = show_editor_document(
                        ui,
                        doc,
                        "a.txt",
                        &font,
                        false,
                        Some(EDITOR_CHUNK_LINES + 1),
                        &EditorSettings::default(),
                    );
                },
                doc,
            );
        harness.run_steps(2);

        let row_height = harness
            .ctx
            .fonts_mut(|fonts| fonts.row_height(&egui::FontId::monospace(13.0)));
        let viewport_top = harness
            .state()
            .last_editor_viewport
            .expect("editor viewport")
            .top();
        let x = 60.0;
        // 视口顶部是块 0 的最后一行，下面才是块 1。从块 1 内按下后向上拖到
        // 视口顶端：旧实现会在块首停住，合并后选区应同时包含两块的行。
        let press = egui::pos2(x, viewport_top + 2.5 * row_height);
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: press,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        let drag = egui::pos2(x, viewport_top + 1.0);
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(drag));
        harness.step();
        // 越过块边界后合并相邻块，随后选区按合并后的文本继续更新。
        harness.step();
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: drag,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();

        let doc = harness.state();
        assert_eq!(doc.chunks.len(), 1, "拖过块边界后相邻块应被合并");
        assert_eq!(doc.chunks[0].id, focused_id);
        assert_eq!(doc.full_text(), text, "合并不得改动全文");
        let state =
            egui::text_edit::TextEditState::load(&harness.ctx, chunk_text_edit_id(focused_id))
                .expect("focused chunk state");
        let range = state.cursor.char_range().expect("selection");
        let sorted = range.as_sorted_char_range();
        assert!(sorted.start.0 < sorted.end.0, "拖选不应在块边界处坍缩");
        let text = &doc.chunks[0].text;
        let line_of = |char_index: usize| {
            text.chars()
                .take(char_index)
                .filter(|ch| *ch == '\n')
                .count()
        };
        let start_line = line_of(sorted.start.0);
        let end_line = line_of(sorted.end.0);
        assert!(
            start_line < EDITOR_CHUNK_LINES && end_line >= EDITOR_CHUNK_LINES,
            "选区应跨过块边界，实际行范围 {start_line}..{end_line}"
        );
        let selected = text
            .chars()
            .skip(sorted.start.0)
            .take(sorted.end.0.saturating_sub(sorted.start.0))
            .collect::<String>();
        assert!(
            selected.contains("line 95") || selected.contains("ne 95"),
            "选区应包含上一块最后一行附近的内容，实际选中: {selected:?}"
        );
        assert!(
            selected.contains("line 96"),
            "选区应包含下一块首行，实际选中: {selected:?}"
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
    fn complete_probe_content_is_reused_without_a_second_download() {
        assert!(probe_contains_complete_file(1024, 1024));
        assert!(probe_contains_complete_file(0, 1024));
        assert!(!probe_contains_complete_file(0, PROBE_BYTES as usize));
        assert!(!probe_contains_complete_file(
            PROBE_BYTES * 2,
            PROBE_BYTES as usize
        ));
    }

    #[test]
    fn search_jump_keeps_two_visual_rows_above_the_match() {
        let row_height = 18.0;
        assert_eq!(
            search_jump_scroll_offset(12.0 * row_height, row_height),
            10.0 * row_height
        );
        assert_eq!(search_jump_scroll_offset(row_height, row_height), 0.0);
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

    #[test]
    fn changing_search_query_resets_the_current_match() {
        let mut search = SearchState {
            open: true,
            query: "foo".into(),
            ..Default::default()
        };
        assert_eq!(ensure_matches("foo foo", 0, &mut search).len(), 2);
        search.current = Some(1);
        search.query = "bar".into();

        assert_eq!(ensure_matches("foo bar", 0, &mut search), &[(4, 7)]);
        assert_eq!(search.current, None);
    }

    #[test]
    fn search_highlights_map_across_chunks_and_mark_the_current_match() {
        let mut lines = (0..(EDITOR_CHUNK_LINES + 8))
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>();
        lines[1] = "needle first".into();
        lines[EDITOR_CHUNK_LINES + 2] = "needle second".into();
        let mut doc = EditorDoc::new(
            1,
            "/tmp/search.txt".into(),
            "search.txt".into(),
            lines.join("\n"),
            false,
        );
        doc.search.open = true;
        doc.search.query = "needle".into();
        let text = doc.full_text();
        assert_eq!(
            ensure_matches(&text, doc.text_gen, &mut doc.search).len(),
            2
        );
        doc.search.current = Some(1);

        let highlights = chunk_search_highlights(&doc);
        assert_eq!(highlights.iter().map(Vec::len).sum::<usize>(), 2);
        assert_eq!(
            highlights
                .iter()
                .flatten()
                .filter(|highlight| highlight.current)
                .count(),
            1
        );
        assert!(highlights.iter().filter(|chunk| !chunk.is_empty()).count() >= 2);
    }

    #[test]
    fn layout_job_marks_all_matches_and_emphasizes_the_current_one() {
        let text = "needle and needle";
        let matches = find_matches(text, "needle", true);
        let highlights = matches
            .iter()
            .enumerate()
            .map(|(index, &(start, end))| SearchHighlightRange {
                start,
                end,
                current: index == 1,
            })
            .collect::<Vec<_>>();
        let mut job = plain_layout_job(
            text,
            &egui::FontId::monospace(13.0),
            Color32::from_rgb(20, 20, 20),
        );

        apply_search_highlights(&mut job, text, &highlights, false);

        let marked = job
            .sections
            .iter()
            .filter(|section| section.format.background != Color32::TRANSPARENT)
            .collect::<Vec<_>>();
        assert_eq!(marked.len(), 2);
        assert_eq!(
            marked
                .iter()
                .map(|section| &text[section.byte_range.start.0..section.byte_range.end.0])
                .collect::<Vec<_>>(),
            vec!["needle", "needle"]
        );
        assert_eq!(
            marked
                .iter()
                .filter(|section| section.format.underline.width > 0.0)
                .count(),
            1
        );
        assert_ne!(marked[0].format.background, marked[1].format.background);
    }

    fn test_font() -> egui::FontId {
        egui::FontId::monospace(13.0)
    }

    #[test]
    fn highlight_state_continuation_keeps_block_comment_across_chunks() {
        let env = HighlightEnv::new("a.rs", true, &EditorSettings::default());
        assert!(!env.plain, "a.rs 应匹配到 Rust 语法");
        let chunk0 = "fn main() {\n    /* 注释开始";
        let chunk1 = "注释结束 */\n}";

        let (job0, end) = highlight_chunk_layout(
            &env,
            chunk0,
            fresh_highlight_states(&env),
            &test_font(),
            Color32::WHITE,
        );
        let (job_continued, _) = highlight_chunk_layout(
            &env,
            chunk1,
            end.expect("高亮返回块末状态"),
            &test_font(),
            Color32::WHITE,
        );
        let (job_fresh, _) = highlight_chunk_layout(
            &env,
            chunk1,
            fresh_highlight_states(&env),
            &test_font(),
            Color32::WHITE,
        );

        let comment_color = job0.sections.last().map(|s| s.format.color);
        let continued_color = job_continued.sections.first().map(|s| s.format.color);
        let fresh_color = job_fresh.sections.first().map(|s| s.format.color);
        assert_eq!(
            continued_color, comment_color,
            "续接高亮时块 1 开头应延续块 0 末尾的注释样式"
        );
        assert_ne!(
            continued_color, fresh_color,
            "从块首重新解析(旧行为)会把注释误判为代码"
        );
    }

    #[test]
    fn advance_only_state_matches_layout_end_state() {
        let env = HighlightEnv::new("a.rs", true, &EditorSettings::default());
        let text = "fn main() {\n    let s = \"hi\";\n    /* 跨块注释\n";
        let (_job, layout_end) = highlight_chunk_layout(
            &env,
            text,
            fresh_highlight_states(&env),
            &test_font(),
            Color32::WHITE,
        );
        let advance_end = advance_highlight_states(&env, text, fresh_highlight_states(&env));
        assert_eq!(
            layout_end.expect("高亮返回块末状态"),
            advance_end,
            "纯解析推进与排版推进必须到达完全相同的块末状态"
        );
    }

    #[test]
    fn highlight_env_uses_configured_theme_with_fallback() {
        let default_env = HighlightEnv::new("a.rs", true, &EditorSettings::default());
        let mut settings = EditorSettings::default();
        settings.highlight_theme_dark = "Solarized (dark)".into();
        let custom = HighlightEnv::new("a.rs", true, &settings);
        assert_ne!(
            custom.theme_bg, default_env.theme_bg,
            "配置的主题应生效(与默认主题背景不同)"
        );
        settings.highlight_theme_dark = "no-such-theme".into();
        let fallback = HighlightEnv::new("a.rs", true, &settings);
        assert_eq!(
            fallback.theme_bg, default_env.theme_bg,
            "未知主题应回退到内建默认"
        );
    }

    #[test]
    fn chunk_hash_tracks_text_changes() {
        let text = (0..EDITOR_CHUNK_LINES + 1)
            .map(|line| format!("line{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut chunks = chunk_text(text);
        assert!(chunks.len() >= 2, "测试需要至少两个块");
        let before: Vec<u64> = chunks.iter().map(|c| c.hash).collect();
        chunks[0].text = "changed".into();
        chunks[0].hash = text_hash(&chunks[0].text);
        assert_ne!(chunks[0].hash, before[0]);
        // 链式折叠:任一块变化都会改变其后的链值
        let chain_before = chain_mix(chain_mix(7, before[0]), before[1]);
        let chain_after = chain_mix(chain_mix(7, chunks[0].hash), chunks[1].hash);
        assert_ne!(chain_before, chain_after);
    }
}
