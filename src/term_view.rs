//! 终端视图:alacritty_terminal 负责终端模拟(网格/ANSI/滚回/选区),
//! 这里负责 egui 渲染、键盘/IME 输入、鼠标选区与滚动。

use std::sync::mpsc::Receiver;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{self, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, Color, CursorShape, NamedColor};
use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke, vec2};

use crate::session::files::SftpEvent;
use crate::session::monitor::SystemMetrics;
use crate::session::ssh::SshParams;
use crate::session::{self, CmdTx, DataRx, HostKeyPrompt, SessionCmd, SessionEvent};

pub struct TermBackground {
    pub color: Color32,
    pub image: Option<egui::TextureHandle>,
    pub image_mode: crate::config::TerminalImageMode,
    pub image_darkness: u8,
}

struct RenderRun {
    row: usize,
    start_col: usize,
    end_col: usize,
    fg: Color32,
    flags: Flags,
    text: String,
    /// 宽字符占位格结束后封闭当前 run，后续文字从绝对列坐标重新开始。
    sealed: bool,
}

struct BackgroundRun {
    row: usize,
    start_col: usize,
    end_col: usize,
    color: Color32,
}

struct CachedTextRun {
    row: usize,
    start_col: usize,
    end_col: usize,
    fg: Color32,
    flags: Flags,
    galley: std::sync::Arc<egui::Galley>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RenderCacheKey {
    revision: u64,
    font_size_bits: u32,
    cell_width_bits: u32,
    cell_height_bits: u32,
    background: Color32,
}

fn paint_terminal_background(painter: &egui::Painter, rect: Rect, background: &TermBackground) {
    painter.rect_filled(rect, 0.0, background.color);

    let Some(texture) = &background.image else {
        return;
    };
    let image_size = texture.size_vec2();
    if image_size.x <= 0.0 || image_size.y <= 0.0 {
        return;
    }

    let full_uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    match background.image_mode {
        crate::config::TerminalImageMode::Stretch => {
            painter.image(texture.id(), rect, full_uv, Color32::WHITE);
        }
        crate::config::TerminalImageMode::Fill => {
            let image_aspect = image_size.x / image_size.y;
            let rect_aspect = rect.width() / rect.height().max(1.0);
            let uv = if image_aspect > rect_aspect {
                let visible = rect_aspect / image_aspect;
                Rect::from_min_max(
                    Pos2::new((1.0 - visible) * 0.5, 0.0),
                    Pos2::new((1.0 + visible) * 0.5, 1.0),
                )
            } else {
                let visible = image_aspect / rect_aspect.max(f32::EPSILON);
                Rect::from_min_max(
                    Pos2::new(0.0, (1.0 - visible) * 0.5),
                    Pos2::new(1.0, (1.0 + visible) * 0.5),
                )
            };
            painter.image(texture.id(), rect, uv, Color32::WHITE);
        }
        crate::config::TerminalImageMode::Fit => {
            let scale = (rect.width() / image_size.x)
                .min(rect.height() / image_size.y)
                .max(0.0);
            let dest = Rect::from_center_size(rect.center(), image_size * scale);
            painter.image(texture.id(), dest, full_uv, Color32::WHITE);
        }
        crate::config::TerminalImageMode::Center => {
            let dest = Rect::from_center_size(rect.center(), image_size);
            painter.image(texture.id(), dest, full_uv, Color32::WHITE);
        }
        crate::config::TerminalImageMode::Tile => {
            let tile_size = image_size.max(egui::vec2(1.0, 1.0));
            let columns = (rect.width() / tile_size.x).ceil() as usize + 1;
            let rows = (rect.height() / tile_size.y).ceil() as usize + 1;
            for row in 0..rows {
                for column in 0..columns {
                    let min = rect.min
                        + egui::vec2(column as f32 * tile_size.x, row as f32 * tile_size.y);
                    let full_dest = Rect::from_min_size(min, tile_size);
                    let dest = full_dest.intersect(rect);
                    if dest.is_positive() {
                        let uv = Rect::from_min_max(
                            Pos2::ZERO,
                            Pos2::new(
                                dest.width() / full_dest.width(),
                                dest.height() / full_dest.height(),
                            ),
                        );
                        painter.image(texture.id(), dest, uv, Color32::WHITE);
                    }
                }
            }
        }
    }

    let alpha = ((background.image_darkness.min(100) as u16 * 255) / 100) as u8;
    if alpha > 0 {
        painter.rect_filled(rect, 0.0, Color32::from_black_alpha(alpha));
    }
}

// ---------------------------------------------------------------------------
// 配色(仿 FinalShell 深蓝底)
// ---------------------------------------------------------------------------

pub const BG: Color32 = Color32::from_rgb(0x1B, 0x2B, 0x3A);
const FG: Color32 = Color32::from_rgb(0xD8, 0xE0, 0xEA);
const CURSOR: Color32 = Color32::from_rgb(0x35, 0xD4, 0x35);
const SELECTION_BG: Color32 = Color32::from_rgb(0x3A, 0x51, 0x68);

/// Tango 风格的 16 色调色板
const ANSI: [Color32; 16] = [
    Color32::from_rgb(0x00, 0x00, 0x00), // black
    Color32::from_rgb(0xCC, 0x00, 0x00), // red
    Color32::from_rgb(0x4E, 0x9A, 0x06), // green
    Color32::from_rgb(0xC4, 0xA0, 0x00), // yellow
    Color32::from_rgb(0x34, 0x65, 0xA4), // blue
    Color32::from_rgb(0x75, 0x50, 0x7B), // magenta
    Color32::from_rgb(0x06, 0x98, 0x9A), // cyan
    Color32::from_rgb(0xD3, 0xD7, 0xCF), // white
    Color32::from_rgb(0x55, 0x57, 0x53), // bright black
    Color32::from_rgb(0xEF, 0x29, 0x29), // bright red
    Color32::from_rgb(0x8A, 0xE2, 0x34), // bright green
    Color32::from_rgb(0xFC, 0xE9, 0x4F), // bright yellow
    Color32::from_rgb(0x72, 0x9F, 0xCF), // bright blue
    Color32::from_rgb(0xAD, 0x7F, 0xA8), // bright magenta
    Color32::from_rgb(0x34, 0xE2, 0xE2), // bright cyan
    Color32::from_rgb(0xEE, 0xEE, 0xEC), // bright white
];

fn indexed_color(i: u8) -> Color32 {
    match i {
        0..=15 => ANSI[i as usize],
        16..=231 => {
            // 6x6x6 颜色立方体
            let levels = [0u8, 95, 135, 175, 215, 255];
            let n = i - 16;
            Color32::from_rgb(
                levels[(n / 36) as usize],
                levels[((n % 36) / 6) as usize],
                levels[(n % 6) as usize],
            )
        }
        _ => {
            // 232..=255 灰阶
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}

fn named_color(n: NamedColor) -> Color32 {
    use NamedColor::*;
    match n {
        Foreground | BrightForeground => FG,
        Background => BG,
        Cursor => CURSOR,
        Black => ANSI[0],
        Red => ANSI[1],
        Green => ANSI[2],
        Yellow => ANSI[3],
        Blue => ANSI[4],
        Magenta => ANSI[5],
        Cyan => ANSI[6],
        White => ANSI[7],
        BrightBlack => ANSI[8],
        BrightRed => ANSI[9],
        BrightGreen => ANSI[10],
        BrightYellow => ANSI[11],
        BrightBlue => ANSI[12],
        BrightMagenta => ANSI[13],
        BrightCyan => ANSI[14],
        BrightWhite => ANSI[15],
        DimBlack => dim(ANSI[0]),
        DimRed => dim(ANSI[1]),
        DimGreen => dim(ANSI[2]),
        DimYellow => dim(ANSI[3]),
        DimBlue => dim(ANSI[4]),
        DimMagenta => dim(ANSI[5]),
        DimCyan => dim(ANSI[6]),
        DimWhite => dim(ANSI[7]),
        DimForeground => dim(FG),
    }
}

fn dim(c: Color32) -> Color32 {
    Color32::from_rgb(
        (c.r() as u16 * 2 / 3) as u8,
        (c.g() as u16 * 2 / 3) as u8,
        (c.b() as u16 * 2 / 3) as u8,
    )
}

fn resolve(c: Color, colors: &Colors, bold: bool) -> Color32 {
    match c {
        Color::Spec(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(i) => {
            let i = if bold && i < 8 { i + 8 } else { i };
            colors[i as usize]
                .map(|rgb| Color32::from_rgb(rgb.r, rgb.g, rgb.b))
                .unwrap_or_else(|| indexed_color(i))
        }
        Color::Named(n) => {
            let n = if bold { n.to_bright() } else { n };
            colors[n]
                .map(|rgb| Color32::from_rgb(rgb.r, rgb.g, rgb.b))
                .unwrap_or_else(|| named_color(n))
        }
    }
}

// ---------------------------------------------------------------------------
// 尺寸
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TermDims {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

// ---------------------------------------------------------------------------
// 事件代理:终端模拟层 → 回写 pty / 剪贴板 / 请求重绘
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct EventProxy {
    /// 共享可替换的输入通道:重连时只换内层 tx,
    /// Term 里持有的 proxy 克隆不用动
    cmd_tx: std::sync::Arc<std::sync::Mutex<CmdTx>>,
    ctx: egui::Context,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(s) => {
                if let Ok(tx) = self.cmd_tx.lock() {
                    let _ = tx.send(SessionCmd::Input(s.into_bytes()));
                }
            }
            // 这些事件全部发生在 UI 正在处理的数据解析、尺寸调整或鼠标操作
            // 过程中；对应路径已经显式标记缓存失效。再次唤醒只会多画一帧。
            Event::Wakeup | Event::CursorBlinkingChange | Event::MouseCursorDirty => {}
            Event::ClipboardStore(_, text) => self.ctx.copy_text(text),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// TermView
// ---------------------------------------------------------------------------

pub struct TermView {
    term: Term<EventProxy>,
    parser: ansi::Processor,
    cmd_tx: CmdTx,
    /// 与 EventProxy 共享的输入通道(reconnect 时替换内层)
    proxy_tx: std::sync::Arc<std::sync::Mutex<CmdTx>>,
    data_rx: DataRx,
    event_rx: Receiver<SessionEvent>,
    monitor_active_tx: Option<tokio::sync::watch::Sender<bool>>,
    exited: bool,
    /// 手动断开置位:网格保留可读,不盖"进程已退出"遮罩
    manual_exit: bool,
    /// 进程自然退出时的遮罩文案;手动断开为 None(网格里有断开标记线)
    exit_overlay: Option<&'static str>,
    dims: TermDims,
    /// 待应用的尺寸与首次请求时间(拖动防抖:拖动窗口时每帧尺寸都在变,
    /// 立刻 resize 会让网格逐帧重排 + 逐帧 SIGWINCH,正在输出的内容
    /// 被反复重画成割裂样式;等尺寸稳定一小段时间再一次性应用)
    pending_dims: Option<(TermDims, std::time::Instant)>,
    /// 滚轮滚动累积器(行数):触控板每帧的增量常不足一行,
    /// 逐帧取整会把小增量丢光,滚动变成"卡一下跳一行";
    /// 累积到满一行再滚,剩余部分留到下帧
    scroll_acc: f32,
    sel_anchor: Option<Point>,
    /// 完成文本选择后，复制浮动按钮左上角附近的位置。
    selection_copy_pos: Option<Pos2>,
    /// 获得输入焦点后控制光标显示/隐藏的闪烁计时。
    cursor_blink_epoch: std::time::Instant,
    cursor_was_focused: bool,
    /// 新建会话首次显示或重新激活标签时，请求一次输入焦点。
    request_initial_focus: bool,
    /// poll() 收到的监控指标,等 app 层取走
    pending_metrics: Option<std::sync::Arc<SystemMetrics>>,
    /// poll() 收到的文件服务事件,等 app 层取走
    pending_sftp: Vec<SftpEvent>,
    /// poll() 收到的主机指纹确认请求,等 app 层取走
    pending_hostkey: Vec<HostKeyPrompt>,
    /// 当前帧的文字 run；只复用 Vec 容量，不复用旧 run 内容和边界。
    render_runs: Vec<RenderRun>,
    /// 缓存的背景 run。终端内容/选区未变化时直接复用。
    background_runs: Vec<BackgroundRun>,
    /// 已完成字体布局的文字 run。光标闪烁时只克隆 Galley 引用，不重新
    /// 拼接字符串或执行 shaping。
    cached_text_runs: Vec<CachedTextRun>,
    render_cache_key: Option<RenderCacheKey>,
    render_revision: u64,
}

impl TermView {
    /// 本地 shell 会话(返回 TermView 与文件服务通道)
    pub fn new(
        ctx: &egui::Context,
        dims: TermDims,
    ) -> anyhow::Result<(Self, session::files::SftpTx)> {
        let ch = session::local::spawn(dims.cols as u16, dims.rows as u16, ctx.clone())?;
        let view = Self::with_channels(
            ctx,
            dims,
            ch.cmd_tx,
            ch.data_rx,
            ch.event_rx,
            ch.monitor_active_tx,
        );
        Ok((view, ch.sftp_tx))
    }

    /// SSH 会话(连接在后台进行,失败信息会显示在终端里)
    pub fn ssh(
        ctx: &egui::Context,
        dims: TermDims,
        params: SshParams,
    ) -> (Self, session::files::SftpTx) {
        let ch = session::ssh::spawn(params, dims.cols as u16, dims.rows as u16, ctx.clone());
        let view = Self::with_channels(
            ctx,
            dims,
            ch.cmd_tx,
            ch.data_rx,
            ch.event_rx,
            ch.monitor_active_tx,
        );
        (view, ch.sftp_tx)
    }

    fn with_channels(
        ctx: &egui::Context,
        dims: TermDims,
        cmd_tx: CmdTx,
        data_rx: DataRx,
        event_rx: Receiver<SessionEvent>,
        monitor_active_tx: Option<tokio::sync::watch::Sender<bool>>,
    ) -> Self {
        let proxy_tx = std::sync::Arc::new(std::sync::Mutex::new(cmd_tx.clone()));
        let proxy = EventProxy {
            cmd_tx: proxy_tx.clone(),
            ctx: ctx.clone(),
        };
        let term = Term::new(Config::default(), &dims, proxy);
        Self {
            term,
            parser: ansi::Processor::new(),
            cmd_tx,
            proxy_tx,
            data_rx,
            event_rx,
            monitor_active_tx,
            exited: false,
            manual_exit: false,
            exit_overlay: None,
            dims,
            pending_dims: None,
            scroll_acc: 0.0,
            sel_anchor: None,
            selection_copy_pos: None,
            cursor_blink_epoch: std::time::Instant::now(),
            cursor_was_focused: false,
            request_initial_focus: true,
            pending_metrics: None,
            pending_sftp: Vec::new(),
            pending_hostkey: Vec::new(),
            render_runs: Vec::with_capacity(128),
            background_runs: Vec::with_capacity(64),
            cached_text_runs: Vec::with_capacity(128),
            render_cache_key: None,
            render_revision: 0,
        }
    }

    /// 排空会话事件,喂给终端模拟器。每帧调用。
    pub fn poll(&mut self) {
        while let Ok(ev) = self.event_rx.try_recv() {
            match ev {
                SessionEvent::Exited => {
                    self.exited = true;
                    // 手动断开已有标记线,不再盖遮罩
                    if !self.manual_exit && self.exit_overlay.is_none() {
                        self.exit_overlay = Some("进程已退出,点击右侧重连按钮重开");
                    }
                }
                SessionEvent::Metrics(m) => self.pending_metrics = Some(m),
                SessionEvent::Sftp(e) => self.pending_sftp.push(e),
                SessionEvent::VerifyHostKey(p) => self.pending_hostkey.push(*p),
            }
        }

        const MAX_BYTES_PER_FRAME: usize = 2 * 1024 * 1024;
        const MAX_PARSE_TIME: std::time::Duration = std::time::Duration::from_millis(4);
        let started = std::time::Instant::now();
        let mut parsed = 0usize;
        let mut changed = false;
        while parsed < MAX_BYTES_PER_FRAME && started.elapsed() < MAX_PARSE_TIME {
            let Ok(data) = self.data_rx.try_recv() else {
                break;
            };
            parsed += data.len();
            self.parser.advance(&mut self.term, &data);
            changed = true;
        }
        if changed {
            self.mark_render_dirty();
        }
        self.data_rx.finish_poll();
    }

    /// 取走积压的监控指标
    pub fn take_metrics(&mut self) -> Option<std::sync::Arc<SystemMetrics>> {
        self.pending_metrics.take()
    }

    /// 取走积压的文件服务事件
    pub fn take_sftp(&mut self) -> Vec<SftpEvent> {
        std::mem::take(&mut self.pending_sftp)
    }

    /// 取走积压的主机指纹确认请求
    pub fn take_hostkey(&mut self) -> Vec<HostKeyPrompt> {
        std::mem::take(&mut self.pending_hostkey)
    }

    /// 当前会话是否正在被主窗口/系统信息页展示。后台 SSH 标签暂停监控，
    /// 状态不变时 watch 不会发送事件，也不会产生每帧通道开销。
    pub fn set_monitor_active(&self, active: bool) {
        if let Some(tx) = &self.monitor_active_tx {
            tx.send_if_modified(|value| {
                if *value == active {
                    false
                } else {
                    *value = active;
                    true
                }
            });
        }
    }

    /// 下一次显示终端时把键盘/IME 焦点交给终端输入区域。
    ///
    /// 标签栏本身也是可点击控件，切换标签后需要显式把点击取得的焦点
    /// 交回新激活的终端。
    pub fn request_focus(&mut self) {
        self.request_initial_focus = true;
    }

    /// 会话进程是否已退出/连接已断开(收到 Exited 或手动断开后置位)
    pub fn is_exited(&self) -> bool {
        self.exited
    }

    /// 当前终端网格尺寸(重连时按它起新 pty)
    pub fn dims(&self) -> TermDims {
        self.dims
    }

    /// 测试用:读出网格全部可见文本
    #[cfg(test)]
    pub fn grid_text(&self) -> String {
        let content = self.term.renderable_content();
        let mut s = String::new();
        let mut last_line = -1i32;
        for indexed in content.display_iter {
            if indexed.point.line.0 != last_line {
                if last_line != -1 {
                    s.push('\n');
                }
                last_line = indexed.point.line.0;
            }
            s.push(indexed.cell.c);
        }
        s
    }

    /// 直接向网格喂一段文本(断开/重连标记线)
    pub fn feed_str(&mut self, s: &str) {
        let data = s.replace('\n', "\r\n").into_bytes();
        self.parser.advance(&mut self.term, &data);
        self.mark_render_dirty();
    }

    /// 手动断开:通知会话线程关闭连接/杀掉子进程,网格内容保留,
    /// 只加一条断开标记线(不盖遮罩,旧输出仍可读可复制)
    pub fn disconnect(&mut self) {
        let _ = self.cmd_tx.send(SessionCmd::Shutdown);
        self.exited = true;
        self.manual_exit = true;
        self.feed_str("\r\n\x1b[90m────── 已断开 ──────\x1b[0m\r\n");
    }

    /// 复用现有网格重连:只换通道,滚回内容完整保留
    pub fn reconnect(
        &mut self,
        cmd_tx: CmdTx,
        data_rx: DataRx,
        event_rx: Receiver<SessionEvent>,
        monitor_active_tx: Option<tokio::sync::watch::Sender<bool>>,
    ) {
        if let Ok(mut guard) = self.proxy_tx.lock() {
            *guard = cmd_tx.clone();
        }
        self.cmd_tx = cmd_tx;
        self.data_rx = data_rx;
        self.event_rx = event_rx;
        self.monitor_active_tx = monitor_active_tx;
        self.exited = false;
        self.manual_exit = false;
        self.exit_overlay = None;
        self.feed_str("\x1b[90m────── 重新连接 ──────\x1b[0m\r\n");
    }

    /// 向 pty 写入字符串(滚动到底部)。
    pub fn send_str(&mut self, s: &str) {
        if self.exited {
            return;
        }
        let _ = self.cmd_tx.send(SessionCmd::Input(s.as_bytes().to_vec()));
        self.term.scroll_display(Scroll::Bottom);
        self.mark_render_dirty();
    }

    fn copy_selection(&mut self, ctx: &egui::Context) {
        if let Some(text) = self.term.selection_to_string() {
            if !text.is_empty() {
                ctx.copy_text(text);
            }
            self.term.selection = None;
            self.selection_copy_pos = None;
            self.mark_render_dirty();
        }
    }

    fn mark_render_dirty(&mut self) {
        self.render_revision = self.render_revision.wrapping_add(1);
        self.render_cache_key = None;
    }

    fn has_selection(&self) -> bool {
        self.term
            .selection
            .as_ref()
            .is_some_and(|selection| !selection.is_empty())
    }

    fn paste(&mut self, text: &str) {
        let bracketed = self.term.mode().contains(TermMode::BRACKETED_PASTE);
        let mut data = String::new();
        if bracketed {
            data.push_str("\x1b[200~");
        }
        data.push_str(&text.replace("\r\n", "\n").replace('\n', "\r"));
        if bracketed {
            data.push_str("\x1b[201~");
        }
        self.send_str(&data);
    }

    // -----------------------------------------------------------------------
    // 渲染与交互
    // -----------------------------------------------------------------------

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        background: &TermBackground,
        font_size: f32,
        right_click_paste: bool,
    ) {
        let font = crate::terminal_font_id(font_size);
        let (cell_w, cell_h) = ui
            .ctx()
            .fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let cell_w = cell_w.max(1.0);
        let cell_h = cell_h.max(1.0);

        let avail = ui.available_size();
        let (response, painter) = ui.allocate_painter(avail, egui::Sense::click_and_drag());
        let window_has_focus = window_has_focus(ui);
        let window_just_gained_focus = ui.input(|input| {
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::WindowFocused(true)))
        });
        let focus_is_unclaimed = ui.memory(|memory| memory.focused().is_none());
        if window_has_focus
            && (self.request_initial_focus || window_just_gained_focus || focus_is_unclaimed)
        {
            response.request_focus();
            self.request_initial_focus = false;
        }
        let rect = response.rect.shrink2(vec2(6.0, 4.0));
        let painter = painter.with_clip_rect(rect);

        // 尺寸变化 → 防抖后调整终端网格与 pty。
        // 渲染始终用 self.dims(旧网格),拖动期间内容保持完整不重排,
        // 只是右侧/底部裁剪或留空;松手尺寸稳定 RESIZE_DEBOUNCE 后一次性 resize
        const RESIZE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);
        let cols = ((rect.width() / cell_w) as usize).max(1);
        let rows = ((rect.height() / cell_h) as usize).max(1);
        let dims = TermDims { cols, rows };
        if dims != self.dims {
            let since = match self.pending_dims {
                Some((d, t)) if d == dims => t,
                _ => {
                    let t = std::time::Instant::now();
                    self.pending_dims = Some((dims, t));
                    t
                }
            };
            if since.elapsed() >= RESIZE_DEBOUNCE {
                self.pending_dims = None;
                self.dims = dims;
                self.term.resize(dims);
                self.mark_render_dirty();
                let _ = self.cmd_tx.send(SessionCmd::Resize {
                    cols: cols as u16,
                    rows: rows as u16,
                });
            } else {
                // 防抖期间没有数据驱动重绘时也要保证到点应用
                ui.ctx()
                    .request_repaint_after(RESIZE_DEBOUNCE - since.elapsed());
            }
        } else {
            self.pending_dims = None;
        }

        paint_terminal_background(&painter, rect, background);

        self.handle_scroll(ui, &response, cell_h);
        self.handle_mouse(ui, &response, rect, cell_w, cell_h, right_click_paste);
        let cursor_blink_visible = self.cursor_blink_visible(ui, response.has_focus());
        self.render_grid(
            &painter,
            rect,
            &font,
            egui::vec2(cell_w, cell_h),
            background.color,
            cursor_blink_visible,
        );

        // IME 候选窗定位到光标处
        if response.has_focus() {
            let content = self.term.renderable_content();
            if let Some(vp) = term::point_to_viewport(content.display_offset, content.cursor.point)
            {
                let cursor_rect = Rect::from_min_size(
                    Pos2::new(
                        rect.left() + vp.column.0 as f32 * cell_w,
                        rect.top() + vp.line as f32 * cell_h,
                    ),
                    vec2(cell_w, cell_h),
                );
                ui.ctx().output_mut(|o| {
                    o.ime = Some(egui::output::IMEOutput {
                        rect: response.rect,
                        cursor_rect,
                        should_interrupt_composition: false,
                    });
                });
            }
        }

        self.handle_keyboard(ui, &response);

        self.show_selection_copy_button(ui, response.id, rect);

        if let Some(msg) = self.exit_overlay {
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(160));
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                msg,
                FontId::proportional(15.0),
                Color32::WHITE,
            );
        }
    }

    fn cursor_blink_visible(&mut self, ui: &egui::Ui, terminal_has_focus: bool) -> bool {
        let focused = terminal_has_focus && window_has_focus(ui);
        if focused != self.cursor_was_focused {
            self.cursor_was_focused = focused;
            self.cursor_blink_epoch = std::time::Instant::now();
        }
        if !focused {
            return false;
        }

        const PHASE: std::time::Duration = std::time::Duration::from_millis(500);
        let elapsed = self.cursor_blink_epoch.elapsed();
        let phase_millis = PHASE.as_millis();
        let within_phase = elapsed.as_millis() % phase_millis;
        let until_change =
            std::time::Duration::from_millis((phase_millis - within_phase).max(1) as u64);
        ui.ctx().request_repaint_after(until_change);
        (elapsed.as_millis() / phase_millis).is_multiple_of(2)
    }

    /// 鼠标滚轮滚回。拖选时也允许滚动；选区锚点使用终端绝对坐标，
    /// 滚动后 `handle_mouse` 会按新的 display_offset 更新活动端点。
    fn handle_scroll(&mut self, ui: &egui::Ui, response: &egui::Response, cell_h: f32) {
        let dragging = response.dragged_by(egui::PointerButton::Primary);
        if response.hovered() || dragging {
            let delta = ui.input(|input| input.smooth_scroll_delta.y);
            if delta != 0.0 {
                self.selection_copy_pos = None;
                self.scroll_acc += delta / cell_h;
                let lines = self.scroll_acc.trunc() as i32;
                if lines != 0 {
                    self.scroll_acc -= lines as f32;
                    self.term.scroll_display(Scroll::Delta(lines));
                    self.mark_render_dirty();
                    ui.ctx().request_repaint();
                }
            }
        } else {
            self.scroll_acc = 0.0;
        }
    }

    /// 按行聚合绘制终端网格
    fn render_grid(
        &mut self,
        painter: &egui::Painter,
        rect: Rect,
        font: &FontId,
        cell_size: egui::Vec2,
        background_color: Color32,
        cursor_blink_visible: bool,
    ) {
        let cell_w = cell_size.x;
        let cell_h = cell_size.y;
        let cache_key = RenderCacheKey {
            revision: self.render_revision,
            font_size_bits: font.size.to_bits(),
            cell_width_bits: cell_w.to_bits(),
            cell_height_bits: cell_h.to_bits(),
            background: background_color,
        };

        if self.render_cache_key != Some(cache_key) {
            let content = self.term.renderable_content();
            let display_offset = content.display_offset;
            let colors = content.colors;

            // 背景与文字使用不同的 run。光标单独绘制，因此 500ms 光标闪烁
            // 不会让整屏文字缓存失效。
            self.render_runs.clear();
            self.background_runs.clear();
            let runs = &mut self.render_runs;
            let backgrounds = &mut self.background_runs;

            for indexed in content.display_iter {
                let point = indexed.point;
                let cell = indexed.cell;
                let row = (point.line.0 + display_offset as i32) as usize;
                let col = point.column.0;
                let selected = content.selection.as_ref().is_some_and(|sel| {
                    sel.contains_cell(&indexed, content.cursor.point, content.cursor.shape)
                });

                let bold = cell.flags.intersects(Flags::BOLD | Flags::DIM_BOLD);
                let mut fg = resolve(cell.fg, colors, bold);
                let mut bg = resolve(cell.bg, colors, false);
                let mut paint_bg = !matches!(cell.bg, Color::Named(NamedColor::Background));
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                    paint_bg = true;
                }
                if cell.flags.contains(Flags::DIM) {
                    fg = dim(fg);
                }
                if selected {
                    bg = SELECTION_BG;
                    paint_bg = true;
                }

                if paint_bg {
                    match backgrounds.last_mut() {
                        Some(run) if run.row == row && run.end_col == col && run.color == bg => {
                            run.end_col = col + 1;
                        }
                        _ => backgrounds.push(BackgroundRun {
                            row,
                            start_col: col,
                            end_col: col + 1,
                            color: bg,
                        }),
                    }
                }

                let ch = if cell.flags.contains(Flags::HIDDEN) {
                    ' '
                } else {
                    cell.c
                };
                let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
                let flags = flags_relevant(cell.flags);

                match runs.last_mut() {
                    Some(run)
                        if run.row == row
                            && run.end_col == col
                            && run.fg == fg
                            && run.flags == flags
                            && !run.sealed =>
                    {
                        if spacer {
                            run.sealed = true;
                        } else {
                            run.text.push(ch);
                        }
                        run.end_col = col + 1;
                    }
                    _ => {
                        if spacer {
                            continue;
                        }
                        // 前导空格不参与 shaping，文字始终从真实网格列开始。
                        if ch == ' ' && !flags.intersects(Flags::UNDERLINE | Flags::STRIKEOUT) {
                            continue;
                        }
                        runs.push(RenderRun {
                            row,
                            start_col: col,
                            end_col: col + 1,
                            fg,
                            flags,
                            text: ch.to_string(),
                            sealed: false,
                        });
                    }
                }
            }

            self.cached_text_runs.clear();
            for run in &self.render_runs {
                if run.text.trim().is_empty() {
                    continue;
                }
                self.cached_text_runs.push(CachedTextRun {
                    row: run.row,
                    start_col: run.start_col,
                    end_col: run.end_col,
                    fg: run.fg,
                    flags: run.flags,
                    galley: painter.layout_no_wrap(run.text.clone(), font.clone(), run.fg),
                });
            }
            self.render_cache_key = Some(cache_key);
        }

        // 先统一绘制全部背景，再复用已 shaping 的 Galley 绘制文字。
        for run in &self.background_runs {
            let x = rect.left() + run.start_col as f32 * cell_w;
            let y = rect.top() + run.row as f32 * cell_h;
            let w = (run.end_col - run.start_col) as f32 * cell_w;
            let run_rect = Rect::from_min_size(Pos2::new(x, y), vec2(w, cell_h));
            painter.rect_filled(run_rect, 0.0, run.color);
        }

        for run in &self.cached_text_runs {
            let x = rect.left() + run.start_col as f32 * cell_w;
            let y = rect.top() + run.row as f32 * cell_h;
            let w = (run.end_col - run.start_col) as f32 * cell_w;
            let run_rect = Rect::from_min_size(Pos2::new(x, y), vec2(w, cell_h));
            painter.galley(Pos2::new(x, y), run.galley.clone(), run.fg);
            if run.flags.contains(Flags::UNDERLINE) {
                painter.line_segment(
                    [run_rect.left_bottom(), run_rect.right_bottom()],
                    Stroke::new(1.0, run.fg),
                );
            }
            if run.flags.contains(Flags::STRIKEOUT) {
                let mid = run_rect.center().y;
                painter.line_segment(
                    [
                        Pos2::new(run_rect.left(), mid),
                        Pos2::new(run_rect.right(), mid),
                    ],
                    Stroke::new(1.0, run.fg),
                );
            }
        }

        let content = self.term.renderable_content();
        let display_offset = content.display_offset;
        let cursor_vp = term::point_to_viewport(display_offset, content.cursor.point);
        let cursor_visible = cursor_blink_visible && content.cursor.shape != CursorShape::Hidden;

        // 光标独立于网格缓存绘制；块状光标覆盖底下的普通文字，再只重画
        // 当前单元格字符，避免光标相位让整屏缓存失效。
        if cursor_visible && let Some(p) = cursor_vp {
            let x = rect.left() + p.column.0 as f32 * cell_w;
            let y = rect.top() + p.line as f32 * cell_h;
            match content.cursor.shape {
                CursorShape::Block => {
                    painter.rect_filled(
                        Rect::from_min_size(Pos2::new(x, y), vec2(cell_w, cell_h)),
                        0.0,
                        CURSOR,
                    );
                    let cell = &self.term.grid()[content.cursor.point];
                    if !cell
                        .flags
                        .intersects(Flags::HIDDEN | Flags::WIDE_CHAR_SPACER)
                        && cell.c != ' '
                    {
                        painter.text(
                            Pos2::new(x, y),
                            Align2::LEFT_TOP,
                            cell.c,
                            font.clone(),
                            background_color,
                        );
                    }
                }
                CursorShape::Beam => {
                    painter.vline(x, y..=y + cell_h, Stroke::new(1.5, CURSOR));
                }
                CursorShape::Underline => {
                    painter.line_segment(
                        [
                            Pos2::new(x, y + cell_h - 1.0),
                            Pos2::new(x + cell_w, y + cell_h - 1.0),
                        ],
                        Stroke::new(1.5, CURSOR),
                    );
                }
                CursorShape::HollowBlock => {
                    painter.rect_stroke(
                        Rect::from_min_size(Pos2::new(x, y), vec2(cell_w, cell_h)),
                        0.0,
                        Stroke::new(1.0, CURSOR),
                        egui::StrokeKind::Inside,
                    );
                }
                _ => {}
            }
        }

        // 滚回指示
        if display_offset > 0 {
            let text = format!("已上翻 {} 行", display_offset);
            painter.text(
                Pos2::new(rect.right() - 6.0, rect.bottom() - 4.0),
                Align2::RIGHT_BOTTOM,
                text,
                FontId::proportional(11.0),
                Color32::from_white_alpha(120),
            );
        }
    }

    // -----------------------------------------------------------------------
    // 鼠标:焦点、选区、复制
    // -----------------------------------------------------------------------

    fn handle_mouse(
        &mut self,
        ui: &mut egui::Ui,
        response: &egui::Response,
        rect: Rect,
        cell_w: f32,
        cell_h: f32,
        right_click_paste: bool,
    ) {
        // 拖到可视区域上下边缘时持续滚动，并在滚动后的视口坐标中更新
        // 选区活动端点。即使鼠标停在边缘不动也要继续请求下一帧。
        if self.sel_anchor.is_some()
            && response.dragged_by(egui::PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
        {
            let lines = selection_autoscroll_lines(pos.y, rect, cell_h);
            if lines != 0 {
                self.term.scroll_display(Scroll::Delta(lines));
                self.mark_render_dirty();
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(16));
            }
        }

        let display_offset = self.term.grid().display_offset();
        let (cols, rows) = (self.dims.cols, self.dims.rows);
        let to_point = |pos: Pos2| -> Point {
            let col = ((pos.x - rect.left()) / cell_w)
                .floor()
                .clamp(0.0, cols as f32 - 1.0) as usize;
            let row = ((pos.y - rect.top()) / cell_h)
                .floor()
                .clamp(0.0, rows as f32 - 1.0) as usize;
            term::viewport_to_point(display_offset, Point::new(row, Column(col)))
        };

        if right_click_paste && response.clicked_by(egui::PointerButton::Secondary) {
            response.request_focus();
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        }

        if response.clicked() {
            response.request_focus();
            self.term.selection = None;
            self.sel_anchor = None;
            self.selection_copy_pos = None;
            self.mark_render_dirty();
        }

        if response.drag_started_by(egui::PointerButton::Primary) {
            self.selection_copy_pos = None;
            if let Some(pos) = response.interact_pointer_pos() {
                let point = to_point(pos);
                self.term.selection =
                    Some(Selection::new(SelectionType::Simple, point, Side::Left));
                self.sel_anchor = Some(point);
                self.mark_render_dirty();
            }
        } else if response.dragged_by(egui::PointerButton::Primary)
            && let (Some(anchor), Some(pos)) = (self.sel_anchor, response.interact_pointer_pos())
        {
            let point = to_point(pos);
            let side = if point < anchor {
                Side::Left
            } else {
                Side::Right
            };
            if let Some(sel) = self.term.selection.as_mut() {
                sel.update(point, side);
                self.mark_render_dirty();
            }
        }

        if response.double_clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let point = to_point(pos);
            self.term.selection = Some(Selection::new(SelectionType::Semantic, point, Side::Left));
            self.selection_copy_pos = Some(pos + vec2(9.0, 9.0));
            self.mark_render_dirty();
        }
        if response.triple_clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let point = to_point(pos);
            self.term.selection = Some(Selection::new(SelectionType::Lines, point, Side::Left));
            self.selection_copy_pos = Some(pos + vec2(9.0, 9.0));
            self.mark_render_dirty();
        }

        if response.drag_stopped_by(egui::PointerButton::Primary) {
            self.sel_anchor = None;
            if self.has_selection() {
                self.selection_copy_pos = response
                    .interact_pointer_pos()
                    .or_else(|| ui.input(|input| input.pointer.latest_pos()))
                    .map(|pos| pos + vec2(9.0, 9.0));
            } else {
                self.selection_copy_pos = None;
            }
        }
    }

    fn show_selection_copy_button(&mut self, ui: &mut egui::Ui, id: egui::Id, term_rect: Rect) {
        let Some(pos) = self.selection_copy_pos else {
            return;
        };
        if !self.has_selection() {
            self.selection_copy_pos = None;
            return;
        }

        let mut copy_clicked = false;
        egui::Area::new(id.with("selection_copy"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .constrain_to(term_rect)
            .movable(false)
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(Color32::from_black_alpha(220))
                    .stroke(Stroke::new(1.0, Color32::from_white_alpha(55)))
                    .corner_radius(5.0)
                    .shadow(egui::Shadow {
                        offset: [0, 2],
                        blur: 7,
                        spread: 0,
                        color: Color32::from_black_alpha(100),
                    })
                    .inner_margin(egui::Margin::same(2))
                    .show(ui, |ui| {
                        copy_clicked = ui
                            .add_sized(
                                [28.0, 24.0],
                                egui::Button::image(
                                    crate::ui::icons::Icon::Copy.image(16.0, Color32::WHITE),
                                )
                                .frame(false),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text("复制")
                            .clicked();
                    });
            });

        if copy_clicked {
            self.copy_selection(ui.ctx());
        }
    }

    // -----------------------------------------------------------------------
    // 键盘 / IME
    // -----------------------------------------------------------------------

    fn handle_keyboard(&mut self, ui: &mut egui::Ui, response: &egui::Response) {
        if !response.has_focus() {
            return;
        }
        // Tab/方向键留在终端内,不触发 egui 焦点导航
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: false,
                },
            );
        });

        let events = ui.input(|i| i.events.clone());
        let ctx = ui.ctx().clone();
        for event in &events {
            match event {
                egui::Event::Text(text) => {
                    self.send_str(text);
                }
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                    self.send_str(text);
                }
                egui::Event::Paste(text) => {
                    self.paste(text);
                }
                egui::Event::Copy => {
                    // macOS 上只会是 Cmd+C;其他平台是 Ctrl+C
                    if self.has_selection() {
                        self.copy_selection(&ctx);
                    } else if !cfg!(target_os = "macos") {
                        self.send_str("\x03");
                    }
                }
                egui::Event::Cut => {
                    if self.has_selection() {
                        self.copy_selection(&ctx);
                    } else if !cfg!(target_os = "macos") {
                        self.send_str("\x18");
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    self.handle_key(*key, modifiers);
                }
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, key: egui::Key, mods: &egui::Modifiers) {
        use egui::Key;

        let app_cursor = self.term.mode().contains(TermMode::APP_CURSOR);
        let seq: Option<Vec<u8>> = match key {
            Key::Enter => Some(b"\r".to_vec()),
            Key::Backspace => Some(if mods.ctrl {
                b"\x08".to_vec()
            } else {
                b"\x7f".to_vec()
            }),
            Key::Tab => Some(if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }),
            Key::Escape => Some(b"\x1b".to_vec()),
            Key::Delete => Some(b"\x1b[3~".to_vec()),
            Key::Insert => Some(b"\x1b[2~".to_vec()),
            Key::Home => Some(b"\x1b[H".to_vec()),
            Key::End => Some(b"\x1b[F".to_vec()),
            Key::ArrowUp => Some(if app_cursor {
                b"\x1bOA".to_vec()
            } else {
                b"\x1b[A".to_vec()
            }),
            Key::ArrowDown => Some(if app_cursor {
                b"\x1bOB".to_vec()
            } else {
                b"\x1b[B".to_vec()
            }),
            Key::ArrowRight => Some(if app_cursor {
                b"\x1bOC".to_vec()
            } else {
                b"\x1b[C".to_vec()
            }),
            Key::ArrowLeft => Some(if app_cursor {
                b"\x1bOD".to_vec()
            } else {
                b"\x1b[D".to_vec()
            }),
            Key::PageUp => {
                self.term.scroll_display(Scroll::PageUp);
                self.mark_render_dirty();
                None
            }
            Key::PageDown => {
                self.term.scroll_display(Scroll::PageDown);
                self.mark_render_dirty();
                None
            }
            _ => {
                // 字母键:Ctrl+字母 → 控制字符;Alt+字母 → ESC 前缀;
                // 普通字母交给 Event::Text 处理
                if let Some(idx) = letter_index(key) {
                    if mods.ctrl {
                        Some(vec![idx + 1]) // Ctrl+A=0x01 … Ctrl+Z=0x1A
                    } else if mods.alt {
                        Some(vec![0x1b, b'a' + idx])
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        if let Some(bytes) = seq {
            let _ = self.cmd_tx.send(SessionCmd::Input(bytes));
            self.term.scroll_display(Scroll::Bottom);
            self.mark_render_dirty();
        }
    }
}

fn window_has_focus(ui: &egui::Ui) -> bool {
    ui.input(|input| input.viewport().focused.unwrap_or(input.raw.focused))
}

/// 影响渲染合并的 flag 位
fn flags_relevant(flags: Flags) -> Flags {
    flags & (Flags::UNDERLINE | Flags::STRIKEOUT)
}

/// 拖选靠近上下边缘时的滚动方向和速度。向上滚回为正，向下为负。
fn selection_autoscroll_lines(pointer_y: f32, rect: Rect, cell_h: f32) -> i32 {
    let edge = cell_h.max(1.0).min(rect.height() * 0.25);
    let distance = if pointer_y < rect.top() + edge {
        rect.top() + edge - pointer_y
    } else if pointer_y > rect.bottom() - edge {
        -(pointer_y - (rect.bottom() - edge))
    } else {
        return 0;
    };
    let speed = (distance.abs() / cell_h.max(1.0)).ceil().clamp(1.0, 6.0) as i32;
    speed * distance.signum() as i32
}

/// 字母键 → 0(A)..=25(Z)
fn letter_index(key: egui::Key) -> Option<u8> {
    use egui::Key;
    Some(match key {
        Key::A => 0,
        Key::B => 1,
        Key::C => 2,
        Key::D => 3,
        Key::E => 4,
        Key::F => 5,
        Key::G => 6,
        Key::H => 7,
        Key::I => 8,
        Key::J => 9,
        Key::K => 10,
        Key::L => 11,
        Key::M => 12,
        Key::N => 13,
        Key::O => 14,
        Key::P => 15,
        Key::Q => 16,
        Key::R => 17,
        Key::S => 18,
        Key::T => 19,
        Key::U => 20,
        Key::V => 21,
        Key::W => 22,
        Key::X => 23,
        Key::Y => 24,
        Key::Z => 25,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn selection_autoscroll_uses_terminal_scroll_direction() {
        let rect = Rect::from_min_max(Pos2::new(0.0, 10.0), Pos2::new(400.0, 210.0));
        assert_eq!(selection_autoscroll_lines(110.0, rect, 20.0), 0);
        assert_eq!(selection_autoscroll_lines(15.0, rect, 20.0), 1);
        assert_eq!(selection_autoscroll_lines(205.0, rect, 20.0), -1);
        assert_eq!(selection_autoscroll_lines(-200.0, rect, 20.0), 6);
        assert_eq!(selection_autoscroll_lines(500.0, rect, 20.0), -6);
    }

    #[test]
    fn cursor_is_hidden_when_window_or_terminal_is_unfocused() {
        let ctx = egui::Context::default();
        let (cmd_tx, _cmd_rx) = session::cmd_channel();
        let (_data_tx, data_rx) = session::data_channel(ctx.clone());
        let (_event_tx, event_rx) = std::sync::mpsc::channel();
        let mut view = TermView::with_channels(
            &ctx,
            TermDims { cols: 80, rows: 24 },
            cmd_tx,
            data_rx,
            event_rx,
            None,
        );

        let mut unfocused_input = egui::RawInput {
            focused: false,
            ..Default::default()
        };
        unfocused_input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .expect("root viewport")
            .focused = Some(false);
        let mut visible_without_window_focus = true;
        let _ = ctx.run_ui(unfocused_input, |ui| {
            visible_without_window_focus = view.cursor_blink_visible(ui, true);
        });
        assert!(!visible_without_window_focus);

        let mut visible_without_terminal_focus = true;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            visible_without_terminal_focus = view.cursor_blink_visible(ui, false);
        });
        assert!(!visible_without_terminal_focus);
    }

    #[test]
    fn terminal_recovers_focus_after_window_interaction_and_activation() {
        let ctx = egui::Context::default();
        crate::install_app_fonts(&ctx, "Hack");
        let (cmd_tx, _cmd_rx) = session::cmd_channel();
        let (_data_tx, data_rx) = session::data_channel(ctx.clone());
        let (_event_tx, event_rx) = std::sync::mpsc::channel();
        let mut view = TermView::with_channels(
            &ctx,
            TermDims { cols: 80, rows: 24 },
            cmd_tx,
            data_rx,
            event_rx,
            None,
        );
        view.request_initial_focus = false;

        let other_id = egui::Id::new("focus-before-window-activation");
        let mut other_input = String::new();
        let background = TermBackground {
            color: BG,
            image: None,
            image_mode: crate::config::TerminalImageMode::Stretch,
            image_darkness: 0,
        };
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.add(egui::TextEdit::singleline(&mut other_input).id(other_id))
                .request_focus();
            view.show(ui, &background, 13.0, false);
        });
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(other_id));

        let mut activation_input = egui::RawInput {
            events: vec![egui::Event::WindowFocused(true)],
            ..Default::default()
        };
        activation_input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .expect("root viewport")
            .focused = Some(true);
        let _ = ctx.run_ui(activation_input, |ui| {
            ui.add(egui::TextEdit::singleline(&mut other_input).id(other_id));
            view.show(ui, &background, 13.0, false);
        });
        let terminal_id = ctx
            .memory(|memory| memory.focused())
            .expect("active terminal should receive focus");
        assert_ne!(terminal_id, other_id);

        ctx.memory_mut(|memory| memory.surrender_focus(terminal_id));
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.add(egui::TextEdit::singleline(&mut other_input).id(other_id));
            view.show(ui, &background, 13.0, false);
        });
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(terminal_id));
    }

    #[test]
    fn tab_activation_can_request_terminal_focus_again() {
        let ctx = egui::Context::default();
        crate::install_app_fonts(&ctx, "Hack");
        let (cmd_tx, _cmd_rx) = session::cmd_channel();
        let (_data_tx, data_rx) = session::data_channel(ctx.clone());
        let (_event_tx, event_rx) = std::sync::mpsc::channel();
        let mut view = TermView::with_channels(
            &ctx,
            TermDims { cols: 80, rows: 24 },
            cmd_tx,
            data_rx,
            event_rx,
            None,
        );
        view.request_initial_focus = false;

        let other_id = egui::Id::new("focus-before-tab-switch");
        let mut other_input = String::new();
        let background = TermBackground {
            color: BG,
            image: None,
            image_mode: crate::config::TerminalImageMode::Stretch,
            image_darkness: 0,
        };
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.add(egui::TextEdit::singleline(&mut other_input).id(other_id))
                .request_focus();
            view.show(ui, &background, 13.0, false);
        });
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(other_id));

        view.request_focus();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.add(egui::TextEdit::singleline(&mut other_input).id(other_id));
            view.show(ui, &background, 13.0, false);
        });

        assert!(!view.request_initial_focus);
        assert_ne!(ctx.memory(|memory| memory.focused()), Some(other_id));
    }

    #[test]
    fn cursor_blink_reuses_cached_terminal_galleys() {
        let ctx = egui::Context::default();
        let (cmd_tx, _cmd_rx) = session::cmd_channel();
        let (_data_tx, data_rx) = session::data_channel(ctx.clone());
        let (_event_tx, event_rx) = std::sync::mpsc::channel();
        let mut view = TermView::with_channels(
            &ctx,
            TermDims { cols: 80, rows: 24 },
            cmd_tx,
            data_rx,
            event_rx,
            None,
        );
        view.feed_str("cached terminal text");

        let mut first_galley = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            view.render_grid(
                ui.painter(),
                Rect::from_min_size(Pos2::ZERO, vec2(640.0, 384.0)),
                &FontId::monospace(13.0),
                vec2(8.0, 16.0),
                BG,
                true,
            );
            first_galley = view.cached_text_runs.first().map(|run| run.galley.clone());
            view.render_grid(
                ui.painter(),
                Rect::from_min_size(Pos2::ZERO, vec2(640.0, 384.0)),
                &FontId::monospace(13.0),
                vec2(8.0, 16.0),
                BG,
                false,
            );
        });
        let first = first_galley.expect("terminal text should create a cached galley");
        let second = &view.cached_text_runs[0].galley;
        assert!(Arc::ptr_eq(&first, second));
    }
}
