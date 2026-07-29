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
            Event::Wakeup | Event::CursorBlinkingChange | Event::MouseCursorDirty => {
                self.ctx.request_repaint();
            }
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
    ctx: egui::Context,
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
    /// 新建会话首次显示终端时请求一次输入焦点。
    request_initial_focus: bool,
    /// poll() 收到的监控指标,等 app 层取走
    pending_metrics: Option<std::sync::Arc<SystemMetrics>>,
    /// poll() 收到的文件服务事件,等 app 层取走
    pending_sftp: Vec<SftpEvent>,
    /// poll() 收到的主机指纹确认请求,等 app 层取走
    pending_hostkey: Vec<HostKeyPrompt>,
    /// 当前帧的文字 run；只复用 Vec 容量，不复用旧 run 内容和边界。
    render_runs: Vec<RenderRun>,
    /// 当前帧的背景 run。与文字 run 分离，选区变化不会重新切分文字。
    background_runs: Vec<BackgroundRun>,
}

impl TermView {
    /// 本地 shell 会话(返回 TermView 与文件服务通道)
    pub fn new(
        ctx: &egui::Context,
        dims: TermDims,
    ) -> anyhow::Result<(Self, session::files::SftpTx)> {
        let ch = session::local::spawn(dims.cols as u16, dims.rows as u16, ctx.clone())?;
        let view = Self::with_channels(ctx, dims, ch.cmd_tx, ch.data_rx, ch.event_rx);
        Ok((view, ch.sftp_tx))
    }

    /// SSH 会话(连接在后台进行,失败信息会显示在终端里)
    pub fn ssh(
        ctx: &egui::Context,
        dims: TermDims,
        params: SshParams,
    ) -> (Self, session::files::SftpTx) {
        let ch = session::ssh::spawn(params, dims.cols as u16, dims.rows as u16, ctx.clone());
        let view = Self::with_channels(ctx, dims, ch.cmd_tx, ch.data_rx, ch.event_rx);
        (view, ch.sftp_tx)
    }

    fn with_channels(
        ctx: &egui::Context,
        dims: TermDims,
        cmd_tx: CmdTx,
        data_rx: DataRx,
        event_rx: Receiver<SessionEvent>,
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
            ctx: ctx.clone(),
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
        while parsed < MAX_BYTES_PER_FRAME && started.elapsed() < MAX_PARSE_TIME {
            let Ok(data) = self.data_rx.try_recv() else {
                break;
            };
            parsed += data.len();
            self.parser.advance(&mut self.term, &data);
        }
        if !self.data_rx.is_empty() {
            self.ctx.request_repaint();
        }
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
    pub fn reconnect(&mut self, cmd_tx: CmdTx, data_rx: DataRx, event_rx: Receiver<SessionEvent>) {
        if let Ok(mut guard) = self.proxy_tx.lock() {
            *guard = cmd_tx.clone();
        }
        self.cmd_tx = cmd_tx;
        self.data_rx = data_rx;
        self.event_rx = event_rx;
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
    }

    fn copy_selection(&mut self, ctx: &egui::Context) {
        if let Some(text) = self.term.selection_to_string() {
            if !text.is_empty() {
                ctx.copy_text(text);
            }
            self.term.selection = None;
            self.selection_copy_pos = None;
        }
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
        if self.request_initial_focus {
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
        let window_has_focus = ui.input(|input| input.viewport().focused.unwrap_or(true));
        let focused = terminal_has_focus && window_has_focus;
        if focused != self.cursor_was_focused {
            self.cursor_was_focused = focused;
            self.cursor_blink_epoch = std::time::Instant::now();
        }
        if !focused {
            return true;
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
        let content = self.term.renderable_content();
        let display_offset = content.display_offset;
        let colors = content.colors;
        let cursor_vp = term::point_to_viewport(display_offset, content.cursor.point);
        let cursor_visible = cursor_blink_visible && content.cursor.shape != CursorShape::Hidden;

        // 背景与文字使用不同的 run。选区只会切分背景，不会改变文字 shaping
        // 的边界，避免缩进、宽字符或字体回退在拖选时发生位移和重绘叠加。
        self.render_runs.clear();
        self.background_runs.clear();
        let runs = &mut self.render_runs;
        let backgrounds = &mut self.background_runs;

        for indexed in content.display_iter {
            let point = indexed.point;
            let cell = indexed.cell;
            let row = (point.line.0 + display_offset as i32) as usize;
            let col = point.column.0;

            let is_cursor =
                cursor_visible && cursor_vp.is_some_and(|p| p.line == row && p.column == col);
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
            if is_cursor && content.cursor.shape == CursorShape::Block {
                fg = background_color;
                bg = CURSOR;
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

            // 宽字符占位格:只并入背景,不追加文本
            let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
            let flags = flags_relevant(cell.flags);

            match runs.last_mut() {
                Some(run)
                    if run.row == row
                        && run.end_col == col
                        && run.fg == fg
                        && run.flags == flags
                        && !run.sealed
                        && !is_cursor =>
                {
                    if spacer {
                        run.sealed = true;
                    } else {
                        run.text.push(ch);
                    }
                    run.end_col = col + 1;
                }
                _ => {
                    // spacer 是前一个宽字符的第二个网格单元，不应成为独立文字。
                    if spacer {
                        continue;
                    }
                    // 不把行首/缩进空格放进待 shaping 的字符串。文字直接从真实
                    // 单元格列开始绘制，选区变化时不会因前导空格重新排版而偏移。
                    if ch == ' ' && !flags.intersects(Flags::UNDERLINE | Flags::STRIKEOUT) {
                        continue;
                    }
                    // 光标单元格强制独立 run,避免颜色被相邻合并后错位
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

        // 先统一绘制全部背景，再绘制文字。否则后一个选区 run 的背景可能覆盖
        // 前一个文字 run 在字形边缘处的像素。
        for run in backgrounds.iter() {
            let x = rect.left() + run.start_col as f32 * cell_w;
            let y = rect.top() + run.row as f32 * cell_h;
            let w = (run.end_col - run.start_col) as f32 * cell_w;
            let run_rect = Rect::from_min_size(Pos2::new(x, y), vec2(w, cell_h));
            painter.rect_filled(run_rect, 0.0, run.color);
        }

        for run in runs.iter() {
            let x = rect.left() + run.start_col as f32 * cell_w;
            let y = rect.top() + run.row as f32 * cell_h;
            let w = (run.end_col - run.start_col) as f32 * cell_w;
            let run_rect = Rect::from_min_size(Pos2::new(x, y), vec2(w, cell_h));
            if !run.text.trim().is_empty() {
                painter.text(
                    Pos2::new(x, y),
                    Align2::LEFT_TOP,
                    &run.text,
                    font.clone(),
                    run.fg,
                );
            }
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

        // 非块状光标
        if cursor_visible && let Some(p) = cursor_vp {
            let x = rect.left() + p.column.0 as f32 * cell_w;
            let y = rect.top() + p.line as f32 * cell_h;
            match content.cursor.shape {
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
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(16));
            }
        }

        let display_offset = self.term.grid().display_offset();
        let to_point = |pos: Pos2| -> Point {
            let col = ((pos.x - rect.left()) / cell_w)
                .floor()
                .clamp(0.0, self.dims.cols as f32 - 1.0) as usize;
            let row = ((pos.y - rect.top()) / cell_h)
                .floor()
                .clamp(0.0, self.dims.rows as f32 - 1.0) as usize;
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
        }

        if response.drag_started_by(egui::PointerButton::Primary) {
            self.selection_copy_pos = None;
            if let Some(pos) = response.interact_pointer_pos() {
                let point = to_point(pos);
                self.term.selection =
                    Some(Selection::new(SelectionType::Simple, point, Side::Left));
                self.sel_anchor = Some(point);
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
            }
        }

        if response.double_clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let point = to_point(pos);
            self.term.selection = Some(Selection::new(SelectionType::Semantic, point, Side::Left));
            self.selection_copy_pos = Some(pos + vec2(9.0, 9.0));
        }
        if response.triple_clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let point = to_point(pos);
            self.term.selection = Some(Selection::new(SelectionType::Lines, point, Side::Left));
            self.selection_copy_pos = Some(pos + vec2(9.0, 9.0));
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
                None
            }
            Key::PageDown => {
                self.term.scroll_display(Scroll::PageDown);
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
        }
    }
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

    #[test]
    fn selection_autoscroll_uses_terminal_scroll_direction() {
        let rect = Rect::from_min_max(Pos2::new(0.0, 10.0), Pos2::new(400.0, 210.0));
        assert_eq!(selection_autoscroll_lines(110.0, rect, 20.0), 0);
        assert_eq!(selection_autoscroll_lines(15.0, rect, 20.0), 1);
        assert_eq!(selection_autoscroll_lines(205.0, rect, 20.0), -1);
        assert_eq!(selection_autoscroll_lines(-200.0, rect, 20.0), 6);
        assert_eq!(selection_autoscroll_lines(500.0, rect, 20.0), -6);
    }
}
