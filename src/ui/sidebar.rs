use crate::app::{MonitorView, ProcSort, RShellApp};
use crate::session::monitor::SystemMetrics;
use egui::{Color32, Pos2, RichText, Stroke, vec2};
use std::collections::VecDeque;
use std::sync::Arc;

fn debug_resize_enabled() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| std::env::var_os("RSHELL_DEBUG_RESIZE").is_some())
}

/// 侧栏宽度范围(Panel::left 的 size_range 与覆盖条 clamp 共用)。
/// 下限必须 ≥ 内容最小宽(约 164:usage_row 32+66+20+间距 16,加分区框
/// 内边距 12+描边 2,加面板边距 16),
/// 否则拖到下限以下时 egui 按内容矩形给右侧面板留位、侧栏却被裁剪显示,
/// 两者之间露出一条背景黑缝
pub(crate) const SIDEBAR_W_MIN: f32 = 168.0;
pub(crate) const SIDEBAR_W_MAX: f32 = 320.0;

impl RShellApp {
    pub(crate) fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        // 原生可拖拽 Panel 会用 hovered/active 的前景色绘制高对比度边线；
        // 浅色主题下该颜色是黑色。外层临时关闭这条线，内容区恢复正常样式，
        // 最后由自定义拖拽条统一绘制一条低对比度分隔线。
        let hovered_fg = ui.visuals().widgets.hovered.fg_stroke;
        let active_fg = ui.visuals().widgets.active.fg_stroke;
        ui.style_mut().visuals.widgets.hovered.fg_stroke = Stroke::NONE;
        ui.style_mut().visuals.widgets.active.fg_stroke = Stroke::NONE;
        egui::Panel::left("sidebar")
            .resizable(true)
            // 默认 234(原 180 基础上 +30%)
            .default_size(234.0)
            .size_range(SIDEBAR_W_MIN..=SIDEBAR_W_MAX)
            .show(ui, |ui| {
                // 仅原生 Panel 分隔线保持隐藏，侧栏内部控件恢复原样。
                ui.style_mut().visuals.widgets.hovered.fg_stroke = hovered_fg;
                ui.style_mut().visuals.widgets.active.fg_stroke = active_fg;
                // 点击"系统信息"标题后置位,绘制结束后打开详细页标签
                let mut open_sysinfo = false;
                // auto_shrink 关掉:宽度始终填满面板,
                // 否则拖宽后 PanelState 存回内容宽度,松手弹回原宽
                let out = egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // 先取快照,避免边栏绘制时与 self 借用冲突。
                        // SysInfo 标签页时显示源会话的数据
                        let (host, metrics, top_mem, top_cpu) = match self.effective_session() {
                            Some(s) => (
                                s.host.clone(),
                                s.monitor.metrics.clone(),
                                Arc::clone(&s.monitor.top_mem),
                                Arc::clone(&s.monitor.top_cpu),
                            ),
                            None => (
                                "—".to_string(),
                                None,
                                Arc::new(Vec::new()),
                                Arc::new(Vec::new()),
                            ),
                        };
                        let has_data = metrics.is_some();

                        // 每个分区套一个圆角包围框,模块边界一眼可辨
                        framed_section(ui, |ui| sidebar_status(ui, &host, has_data));
                        framed_section(ui, |ui| {
                            if sidebar_sysinfo(ui, metrics.as_deref()) {
                                open_sysinfo = true;
                            }
                        });
                        framed_section(ui, |ui| {
                            sidebar_processes(
                                ui,
                                metrics.as_deref(),
                                &top_mem,
                                &top_cpu,
                                &mut self.proc_sort,
                            )
                        });
                        // 网络卡片需要改选中网卡,直接操作会话的 monitor(不走快照)
                        framed_section(ui, |ui| match self.effective_session_idx() {
                            Some(idx) => sidebar_network(ui, &mut self.sessions[idx].monitor),
                            None => sidebar_network(ui, &mut MonitorView::default()),
                        });
                        framed_section(ui, |ui| sidebar_disks(ui, metrics.as_deref()));
                    });
                if debug_resize_enabled() {
                    eprintln!(
                        "scrollarea: id={} inner_rect={:?} content_size={:?}",
                        out.id.short_debug_format(),
                        out.inner_rect,
                        out.content_size
                    );
                }
                if open_sysinfo {
                    self.open_sysinfo_tab();
                }
            });
        ui.style_mut().visuals.widgets.hovered.fg_stroke = hovered_fg;
        ui.style_mut().visuals.widgets.active.fg_stroke = active_fg;
    }

    /// 侧栏右缘的拖拽覆盖条,必须在所有面板之后调用。
    ///
    /// egui 原生把手以边框为中心左右各 3px,但终端的 painter 占满中央区且
    /// 后注册(同层后者赢),把手右半边的点击全被终端吃掉,只剩 3px 能拖。
    /// 这里在最后补一条 10px 命中条(后注册必然赢),拖动时直接写 PanelState,
    /// 下一帧 Panel 读取存储的宽度完成缩放(内容已 auto_shrink(false) 填满,
    /// 宽度不会回弹)。
    pub(crate) fn show_sidebar_resize_grip(&mut self, ui: &mut egui::Ui) {
        use egui::containers::panel::PanelState;
        let panel_id = egui::Id::new("sidebar");
        let Some(state) = PanelState::load(ui.ctx(), panel_id) else {
            return;
        };
        let left = state.outer_rect.left();
        let edge = state.outer_rect.right();
        let strip = egui::Rect::from_min_max(
            egui::pos2(edge - 5.0, state.outer_rect.top()),
            egui::pos2(edge + 5.0, state.outer_rect.bottom()),
        );
        let resp = ui.interact(
            strip,
            panel_id.with("resize_grip"),
            egui::Sense::click_and_drag(),
        );
        // 高亮线位置:拖拽时跟到 clamp 后的目标宽,指针越过上下限也不动,
        // 这样拖到极限后宽度和高亮都停住,不会露出背景缝
        let mut hi_edge = edge;
        if resp.dragged()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let w = (pos.x - left).clamp(SIDEBAR_W_MIN, SIDEBAR_W_MAX);
            hi_edge = left + w;
            let mut rect = state.outer_rect;
            rect.max.x = left + w;
            ui.ctx()
                .data_mut(|d| d.insert_persisted(panel_id, PanelState { outer_rect: rect }));
        }
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            // 仅绘制一条低对比度细线，避免默认黑色高亮压在终端之上。
            ui.painter().vline(
                hi_edge,
                strip.y_range(),
                Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 格式化辅助
// ---------------------------------------------------------------------------

/// 侧栏分区的圆角包围框:描边 + 内边距,宽度填满侧栏
fn framed_section(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .stroke(Stroke::new(
            1.0,
            ui.visuals().widgets.noninteractive.bg_stroke.color,
        ))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 5))
        .show(ui, |ui| {
            // 内容填满框宽,否则框随内容收缩、各框宽窄不一
            ui.set_min_width(ui.available_width());
            add(ui);
        });
    ui.add_space(4.0);
}

/// 381M / 1.7G / 512K
pub fn fmt_kb(kb: u64) -> String {
    const K: u64 = 1024;
    if kb >= K * K * K {
        format!("{:.1}T", kb as f64 / (K * K * K) as f64)
    } else if kb >= K * K {
        // G 级在 10 以内给两位小数:进程内存波动是 MB 级,
        // 一位小数会被舍入吞掉,列表看起来"不刷新"
        let g = kb as f64 / (K * K) as f64;
        if g < 10.0 {
            format!("{g:.2}G")
        } else {
            format!("{g:.1}G")
        }
    } else if kb >= K {
        format!("{:.0}M", kb as f64 / K as f64)
    } else {
        format!("{kb}K")
    }
}

/// 字节数 → "3.6K" / "602B" / "12M"
pub fn fmt_bytes(b: u64) -> String {
    if b >= 1024 * 1024 * 1024 {
        format!("{:.1}G", b as f64 / 1024.0 / 1024.0 / 1024.0)
    } else if b >= 1024 * 1024 {
        format!("{:.1}M", b as f64 / 1024.0 / 1024.0)
    } else if b >= 1024 {
        format!("{:.1}K", b as f64 / 1024.0)
    } else {
        format!("{b}B")
    }
}

/// 每秒字节数 → "410K"/"1.2M"
pub fn fmt_rate(bps: f64) -> String {
    if bps >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1}G", bps / 1024.0 / 1024.0 / 1024.0)
    } else if bps >= 1024.0 * 1024.0 {
        format!("{:.1}M", bps / 1024.0 / 1024.0)
    } else if bps >= 1024.0 {
        format!("{:.0}K", bps / 1024.0)
    } else {
        format!("{:.0}B", bps)
    }
}

fn fmt_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{days} 天")
    } else if hours > 0 {
        format!("{hours} 小时 {mins} 分")
    } else {
        format!("{mins} 分钟")
    }
}

fn pct(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (used as f32 / total as f32).clamp(0.0, 1.0)
    }
}

/// 系统资源占用率颜色：低于 70% 正常，70% 起警告，90% 起危险。
/// CPU、物理内存和交换空间共用，避免百分比与颜色表达相互矛盾。
fn usage_color(frac: f32) -> Color32 {
    if frac >= 0.9 {
        Color32::from_rgb(0xE8, 0x4C, 0x3D)
    } else if frac >= 0.7 {
        Color32::from_rgb(0xF1, 0xA6, 0x3C)
    } else {
        Color32::from_rgb(0x4C, 0xAF, 0x50)
    }
}

// ---------------------------------------------------------------------------
// 各分区
// ---------------------------------------------------------------------------

/// 同步状态 / IP
fn sidebar_status(ui: &mut egui::Ui, host: &str, has_data: bool) {
    ui.horizontal(|ui| {
        ui.label("状态");
        ui.label(RichText::new("●").color(if has_data {
            Color32::from_rgb(0x35, 0xC4, 0x50)
        } else {
            Color32::DARK_GRAY
        }));
    });
    ui.horizontal(|ui| {
        ui.label("IP");
        // 主机名可能很长,截断显示以免撑高内容最小宽
        ui.add(egui::Label::new(RichText::new(host).strong()).truncate());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if sidebar_copy_button(ui).clicked() {
                ui.ctx().copy_text(host.to_string());
            }
        });
    });
}

/// 固定几何尺寸的侧栏复制按钮。
///
/// 标准 `small_button` 会根据上一帧的 hover 状态选取不同的 Frame 样式；
/// 在紧贴自适应卡片底部时，状态边距可能反向撑大父布局。这里把交互态只用于
/// 颜色，描边始终画在固定矩形内部，避免同步状态卡片悬停时改变高度。
fn sidebar_copy_button(ui: &mut egui::Ui) -> egui::Response {
    let text = "复制";
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text_size = ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(text.to_owned(), font.clone(), ui.visuals().text_color())
            .size()
    });
    let size = vec2((text_size.x + 10.0).ceil(), text_size.y.ceil() + 2.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = ui.style().interact(&response);
    ui.painter().rect(
        rect,
        egui::CornerRadius::same(3),
        visuals.weak_bg_fill,
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        font,
        visuals.fg_stroke.color,
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("复制 IP 地址")
}

/// 系统信息:运行时间、负载、CPU/内存/交换。
/// 标题是带边框和填充的按钮(一眼可知可点),返回是否被点击
fn sidebar_sysinfo(ui: &mut egui::Ui, m: Option<&SystemMetrics>) -> bool {
    let open_detail = ui
        .add(egui::Button::new("系统信息").frame_when_inactive(true))
        .on_hover_text("打开详细系统信息页")
        .clicked();
    egui::Grid::new("sysinfo_grid").show(ui, |ui| {
        ui.label("运行");
        ui.label(m.map(|m| fmt_uptime(m.uptime_secs)).unwrap_or("—".into()));
        ui.end_row();
        ui.label("负载");
        ui.label(
            m.map(|m| format!("{:.2}, {:.2}, {:.2}", m.load[0], m.load[1], m.load[2]))
                .unwrap_or("—".into()),
        );
        ui.end_row();
    });

    // 三行的文本列宽统一,保证进度条长度一致(否则文本长的行进度条短,
    // 文本短的行进度条长,参差不齐)
    let cpu_text = m
        .map(|mm| format!("{:.0}%", mm.cpu_percent))
        .unwrap_or_else(|| "—".into());
    let mem_text = m
        .map(|mm| format!("{}/{}", fmt_kb(mm.mem_used_kb), fmt_kb(mm.mem_total_kb)))
        .unwrap_or_else(|| "—".into());
    let swap_text = m
        .map(|mm| format!("{}/{}", fmt_kb(mm.swap_used_kb), fmt_kb(mm.swap_total_kb)))
        .unwrap_or_else(|| "—".into());
    let font = egui::TextStyle::Body.resolve(ui.style());
    let measure = |s: &str| {
        ui.fonts_mut(|f| {
            f.layout_no_wrap(s.to_string(), font.clone(), ui.visuals().text_color())
                .size()
                .x
        })
    };
    let text_w = measure(&cpu_text)
        .max(measure(&mem_text))
        .max(measure(&swap_text));

    let cpu = m.map(|mm| mm.cpu_percent / 100.0).unwrap_or(0.0);
    usage_row(ui, "CPU", cpu, cpu_text, usage_color(cpu), text_w);
    let mem_frac = m
        .map(|mm| pct(mm.mem_used_kb, mm.mem_total_kb))
        .unwrap_or(0.0);
    usage_row(
        ui,
        "内存",
        mem_frac,
        mem_text,
        usage_color(mem_frac),
        text_w,
    );
    let swap_frac = m
        .map(|mm| pct(mm.swap_used_kb, mm.swap_total_kb))
        .unwrap_or(0.0);
    usage_row(
        ui,
        "交换",
        swap_frac,
        swap_text,
        usage_color(swap_frac),
        text_w,
    );
    open_detail
}

/// label 定宽 + 文本列定宽(text_w)+ bar 吃剩余。
/// 三行共用同一个 text_w,进度条长度一致。
/// 注意:ProgressBar 默认会占满剩余宽度,后面再跟文本会超出面板,
/// 导致 PanelState 膨胀到 size_range 上限(拖拽被顶死)
fn usage_row(ui: &mut egui::Ui, label: &str, frac: f32, text: String, color: Color32, text_w: f32) {
    ui.horizontal(|ui| {
        let avail = ui.available_width();
        let label_w = 32.0;
        let spacing = ui.spacing().item_spacing.x;
        // 百分比使用 TextStyle::Button，区域高度跟随内置字体的真实行高。
        let row_h = ui
            .text_style_height(&egui::TextStyle::Body)
            .max(ui.text_style_height(&egui::TextStyle::Button))
            .ceil();
        // 保留文字所在行的高度，只缩细进度条并由 horizontal 布局垂直居中。
        // 直接压缩整行会再次造成百分比文字上下偏移。
        let bar_h = row_h * (2.0 / 3.0);
        // 侧栏过窄时:先保 bar 下限 20,文本按可用截断
        let text_w = text_w.min((avail - label_w - 20.0 - 2.0 * spacing).max(0.0));
        let bar_w = (avail - label_w - text_w - 2.0 * spacing).max(20.0);
        ui.add_sized([label_w, row_h], egui::Label::new(label));
        let bar = egui::ProgressBar::new(frac)
            .fill(color)
            .show_percentage()
            .desired_width(bar_w)
            .desired_height(bar_h);
        ui.add(bar);
        ui.add_sized([text_w, row_h], egui::Label::new(text).truncate());
    });
}

/// 按排序键取前 10(采集层给的是全量进程,排序/截断都在这里做)
#[cfg(test)]
pub fn top_procs(m: &SystemMetrics, sort: ProcSort) -> Vec<&crate::session::monitor::ProcInfo> {
    let mut procs: Vec<_> = m.procs.iter().collect();
    match sort {
        ProcSort::Mem => procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb)),
        ProcSort::Cpu => procs.sort_by(|a, b| {
            b.cpu
                .partial_cmp(&a.cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
    }
    procs.truncate(10);
    procs
}

fn process_text_label(text: impl Into<egui::WidgetText>) -> egui::Label {
    // 可拖选文字，但悬停光标由调用处覆盖成箭头。
    egui::Label::new(text).selectable(true)
}

/// 进程列表(内存/CPU/进程);进程列弹性宽,整行最小宽 ≈ 56+36+20+间距,
/// 保证内容最小宽低于 SIDEBAR_W_MIN。
/// 点击"内存"/"CPU"表头切换排序键,选中列高亮;始终显示该键的前 10 名
/// (采集层给的是全量进程,高 CPU 低内存的进程不会被内存截断漏掉)
fn sidebar_processes(
    ui: &mut egui::Ui,
    m: Option<&SystemMetrics>,
    top_mem: &[crate::session::monitor::ProcInfo],
    top_cpu: &[crate::session::monitor::ProcInfo],
    sort: &mut ProcSort,
) {
    let spacing = ui.spacing().item_spacing.x;
    let cmd_w = (ui.available_width() - 56.0 - 36.0 - 2.0 * spacing).max(20.0);
    ui.horizontal(|ui| {
        let cols = [
            (
                "内存",
                56.0,
                ProcSort::Mem,
                Color32::from_rgb(0x3D, 0x7E, 0xE8),
            ),
            (
                "CPU",
                36.0,
                ProcSort::Cpu,
                Color32::from_rgb(0xE8, 0x4C, 0x3D),
            ),
        ];
        for (t, w, key, c) in cols {
            // 选中列提亮底色,提示当前排序键
            let bg = if *sort == key {
                c.gamma_multiply(1.5)
            } else {
                c
            };
            let (rect, resp) = ui.allocate_exact_size(vec2(w, 16.0), egui::Sense::click());
            if resp.clicked() {
                *sort = key;
            }
            resp.on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!("按{t}排序"));
            ui.painter().rect_filled(rect, 0.0, bg);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                t,
                egui::FontId::proportional(11.0),
                Color32::WHITE,
            );
        }
        let (rect, _) = ui.allocate_exact_size(vec2(cmd_w, 16.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 0.0, Color32::from_rgb(0x5B, 0x9B, 0xD5));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "进程",
            egui::FontId::proportional(11.0),
            Color32::WHITE,
        );
    });
    match m {
        Some(m) if !m.procs.is_empty() => {
            let procs = match sort {
                ProcSort::Mem => top_mem,
                ProcSort::Cpu => top_cpu,
            };
            for p in procs {
                ui.horizontal(|ui| {
                    let row_h = ui
                        .text_style_height(&egui::TextStyle::Body)
                        .ceil()
                        .max(14.0);
                    ui.add_sized([56.0, row_h], process_text_label(fmt_kb(p.rss_kb)))
                        .on_hover_cursor(egui::CursorIcon::Default);
                    ui.add_sized(
                        [36.0, row_h],
                        // 一位小数:0.3% 这类波动也能看到,整数会常年显示 0
                        process_text_label(format!("{:.1}", p.cpu)),
                    )
                    .on_hover_cursor(egui::CursorIcon::Default);
                    ui.add_sized([cmd_w, row_h], process_text_label(&p.command).truncate())
                        .on_hover_cursor(egui::CursorIcon::Default);
                });
            }
        }
        _ => {
            ui.add(process_text_label(
                RichText::new("（无数据）").small().weak(),
            ))
            .on_hover_cursor(egui::CursorIcon::Default);
        }
    }
}

/// 网络速率、折线图、延迟;右上角下拉可切换监控的网卡(None = 全部聚合)
fn sidebar_network(ui: &mut egui::Ui, monitor: &mut MonitorView) {
    let metrics = monitor.metrics.as_ref();
    // 当前选中网卡的速率(选中项不存在时回退聚合值)
    let (rx, tx) = match (&monitor.net_sel, metrics) {
        (Some(name), Some(m)) => m
            .nets
            .iter()
            .find(|n| &n.name == name)
            .map(|n| (n.rx_bps, n.tx_bps))
            .unwrap_or((m.net_rx_bps, m.net_tx_bps)),
        (None, Some(m)) => (m.net_rx_bps, m.net_tx_bps),
        _ => (0.0, 0.0),
    };
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("↑{}", fmt_rate(tx))).color(Color32::from_rgb(0xC4, 0x28, 0x1C)),
        );
        ui.label(
            RichText::new(format!("↓{}", fmt_rate(rx))).color(Color32::from_rgb(0x1C, 0x8B, 0x4A)),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 网卡选择:全部 + 各网卡
            let cur = monitor.net_sel.clone().unwrap_or_else(|| "全部".into());
            let names: Vec<String> = metrics
                .map(|m| m.nets.iter().map(|n| n.name.clone()).collect())
                .unwrap_or_default();
            ui.menu_button(cur, |ui| {
                if ui
                    .selectable_label(monitor.net_sel.is_none(), "全部")
                    .clicked()
                {
                    monitor.net_sel = None;
                    ui.close();
                }
                for name in names {
                    let selected = monitor.net_sel.as_deref() == Some(name.as_str());
                    if ui.selectable_label(selected, &name).clicked() {
                        monitor.net_sel = Some(name);
                        ui.close();
                    }
                }
            });
        });
    });

    // 双折线图:rx 绿 / tx 红,共用一个最大值做缩放
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    // 背景和辅助线跟随当前主题；原先写死的浅色会在深色主题下形成白块。
    let chart_bg = ui.visuals().extreme_bg_color;
    let grid_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    p.rect_filled(rect, 2.0, chart_bg);
    for frac in [0.25, 0.5, 0.75] {
        let y = rect.top() + rect.height() * frac;
        p.hline(rect.left()..=rect.right(), y, Stroke::new(1.0, grid_color));
    }
    // 折线图数据源:选中网卡的历史,未选则用聚合历史
    let empty_hist = VecDeque::new();
    let hist: &VecDeque<(f64, f64)> = match &monitor.net_sel {
        Some(name) => monitor.net_histories.get(name).unwrap_or(&empty_hist),
        None => &monitor.net_history,
    };
    if hist.len() >= 2 {
        let max = hist.iter().map(|(r, t)| r.max(*t)).fold(1.0f64, f64::max);
        let n = hist.len();
        let to_points = |f: fn(&(f64, f64)) -> &f64| -> Vec<Pos2> {
            hist.iter()
                .enumerate()
                .map(|(i, v)| {
                    let x = rect.left() + rect.width() * i as f32 / (n - 1) as f32;
                    let h = (*f(v) / max) as f32 * (rect.height() - 4.0);
                    Pos2::new(x, rect.bottom() - 2.0 - h)
                })
                .collect()
        };
        let rx_pts = to_points(|v| &v.0);
        let tx_pts = to_points(|v| &v.1);
        p.add(egui::Shape::line(
            tx_pts,
            Stroke::new(1.5, Color32::from_rgb(0xE0, 0x6C, 0x50)),
        ));
        p.add(egui::Shape::line(
            rx_pts,
            Stroke::new(1.5, Color32::from_rgb(0x35, 0xA8, 0x60)),
        ));
    }

    ui.horizontal(|ui| {
        let rtt = monitor
            .metrics
            .as_ref()
            .map(|m| format!("{}ms", m.rtt_ms))
            .unwrap_or_else(|| "—".into());
        ui.label(rtt);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(monitor.metrics.as_ref().map(|_| "").unwrap_or("未连接"));
        });
    });
}

/// 磁盘列表(路径 可用/大小)
fn sidebar_disks(ui: &mut egui::Ui, m: Option<&SystemMetrics>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("路径").weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new("可用/大小").weak());
        });
    });
    match m {
        Some(m) if m.disks.iter().any(|d| d.is_real()) => {
            // 侧栏只显示真实设备,虚拟文件系统(tmpfs/overlay…)在详细页里看
            for d in m.disks.iter().filter(|d| d.is_real()) {
                let frac = d.usage_frac();
                // 使用率越高底色越红
                let bg = if frac > 0.9 {
                    Color32::from_rgb(0xF5, 0xC6, 0xC0)
                } else if frac > 0.7 {
                    Color32::from_rgb(0xF7, 0xE3, 0xB8)
                } else {
                    Color32::TRANSPARENT
                };
                let w = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(vec2(w, 18.0), egui::Sense::hover());
                let size_text = format!("{}/{}", fmt_kb(d.avail_kb), fmt_kb(d.total_kb));
                // 路径可能被裁剪,悬停显示完整路径
                resp.on_hover_text(format!("{}\n{size_text}", d.path));
                if bg != Color32::TRANSPARENT {
                    ui.painter().rect_filled(rect, 0.0, bg);
                }
                // 占用比例条
                let fill_w = rect.width() * frac;
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(
                        rect.left_bottom() - vec2(0.0, 2.0),
                        vec2(fill_w, 2.0),
                    ),
                    0.0,
                    Color32::from_rgb(0x7E, 0xA6, 0xD8),
                );
                ui.painter().text(
                    rect.right_center() - vec2(2.0, 0.0),
                    egui::Align2::RIGHT_CENTER,
                    &size_text,
                    egui::FontId::proportional(11.5),
                    ui.visuals().weak_text_color(),
                );
                // 路径裁剪到尺寸文本左侧,超长路径不再与可用空间重叠
                let size_w = ui.fonts_mut(|f| {
                    f.layout_no_wrap(
                        size_text.clone(),
                        egui::FontId::proportional(11.5),
                        ui.visuals().weak_text_color(),
                    )
                    .size()
                    .x
                });
                let path_clip = egui::Rect::from_min_max(
                    rect.min,
                    egui::pos2(rect.right() - size_w - 8.0, rect.bottom()),
                );
                ui.painter().with_clip_rect(path_clip).text(
                    rect.left_center() + vec2(2.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    &d.path,
                    egui::FontId::proportional(11.5),
                    ui.visuals().text_color(),
                );
            }
        }
        _ => {
            ui.label(RichText::new("（无数据）").small().weak());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::monitor::{ProcInfo, SystemMetrics};
    use egui_kittest::kittest::Queryable;

    fn procs(spec: &[(u64, f32, &str)]) -> SystemMetrics {
        SystemMetrics {
            procs: spec
                .iter()
                .map(|&(rss_kb, cpu, command)| ProcInfo {
                    rss_kb,
                    cpu,
                    command: command.into(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn top_procs_by_mem() {
        // 高 CPU 低内存的进程在内存排序下进不了前 10 之外的也应排后
        let m = procs(&[
            (100, 90.0, "cpu_hog"),
            (9000, 0.1, "mem_hog"),
            (500, 1.0, "mid"),
        ]);
        let top = top_procs(&m, ProcSort::Mem);
        assert_eq!(top[0].command, "mem_hog");
        assert_eq!(top[1].command, "mid");
        assert_eq!(top[2].command, "cpu_hog");
    }

    #[test]
    fn top_procs_by_cpu() {
        let m = procs(&[
            (100, 90.0, "cpu_hog"),
            (9000, 0.1, "mem_hog"),
            (500, 1.0, "mid"),
        ]);
        let top = top_procs(&m, ProcSort::Cpu);
        assert_eq!(top[0].command, "cpu_hog");
        assert_eq!(top[1].command, "mid");
        assert_eq!(top[2].command, "mem_hog");
    }

    #[test]
    fn top_procs_truncates_to_10() {
        let spec: Vec<(u64, f32, String)> = (0..20)
            .map(|i| (i as u64, i as f32, format!("p{i}")))
            .collect();
        let m = procs(
            &spec
                .iter()
                .map(|(r, c, n)| (*r, *c, n.as_str()))
                .collect::<Vec<_>>(),
        );
        assert_eq!(top_procs(&m, ProcSort::Mem).len(), 10);
        assert_eq!(top_procs(&m, ProcSort::Cpu).len(), 10);
    }

    #[test]
    fn process_text_uses_arrow_cursor() {
        let metrics = procs(&[(1024, 0.5, "sshd")]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(220.0, 100.0))
            .build_ui_state(
                |ui, state: &mut (SystemMetrics, ProcSort)| {
                    let (metrics, sort) = state;
                    let rows = metrics.procs.clone();
                    sidebar_processes(ui, Some(metrics), &rows, &rows, sort);
                },
                (metrics, ProcSort::Mem),
            );

        let command_rect = harness.get_by_label("sshd").rect();
        harness.hover_at(command_rect.center());
        harness.run();
        assert_eq!(
            harness.output().platform_output.cursor_icon,
            egui::CursorIcon::Default
        );
    }

    #[test]
    fn process_text_remains_selectable() {
        let ctx = egui::Context::default();
        let mut senses_drag = false;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            senses_drag = ui.add(process_text_label("sshd")).sense.senses_drag();
        });
        assert!(senses_drag, "selectable process text should sense drags");
    }

    #[test]
    fn resource_usage_color_tracks_thresholds() {
        assert_eq!(usage_color(0.0), Color32::from_rgb(0x4C, 0xAF, 0x50));
        assert_eq!(usage_color(0.69), Color32::from_rgb(0x4C, 0xAF, 0x50));
        assert_eq!(usage_color(0.70), Color32::from_rgb(0xF1, 0xA6, 0x3C));
        assert_eq!(usage_color(0.89), Color32::from_rgb(0xF1, 0xA6, 0x3C));
        assert_eq!(usage_color(0.90), Color32::from_rgb(0xE8, 0x4C, 0x3D));
        assert_eq!(usage_color(1.0), Color32::from_rgb(0xE8, 0x4C, 0x3D));
    }
}
