//! 系统信息详细页(SysInfo 标签页内容,布局参考 FinalShell):
//! 基本信息 → CPU → CPU 占用分类 → 内存/交换 → 网络接口表 → 文件系统表

use crate::app::RShellApp;
use crate::session::monitor::SystemMetrics;
use crate::ui::sidebar::{fmt_bytes, fmt_kb, fmt_rate};
use egui::RichText;

/// 行首标签列宽
const LABEL_W: f32 = 80.0;

impl RShellApp {
    pub(crate) fn show_sysinfo_page(&mut self, ui: &mut egui::Ui) {
        // 白底黑字:中央面板是终端深蓝底,深色主题文字在上面发灰看不清
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, egui::Color32::WHITE);
        ui.visuals_mut().override_text_color = Some(egui::Color32::from_rgb(0x1A, 0x1A, 0x1A));

        // SysInfo 标签:数据来自源会话的监控快照
        let Some(monitor) = self.effective_session().map(|s| s.monitor.clone()) else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("源会话已关闭").weak());
            });
            return;
        };
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(10.0);
                match monitor.metrics.as_ref() {
                    Some(m) => sysinfo_detail(ui, m),
                    None => {
                        ui.add_space(20.0);
                        ui.label(RichText::new("等待监控数据…").weak());
                    }
                }
            });
    }
}

fn sysinfo_detail(ui: &mut egui::Ui, m: &SystemMetrics) {
    // ---- 基本信息 ----
    detail_section(ui, |ui| {
        info_row(ui, "操作系统", or_dash(&m.os_pretty));
        info_row(ui, "内核", or_dash(&m.kernel_name));
        info_row(ui, "内核版本", or_dash(&m.kernel_release));
        info_row(ui, "硬件架构", or_dash(&m.arch));
        info_row(ui, "主机名称", or_dash(&m.hostname));
    });
    ui.add_space(10.0);

    // ---- CPU ----
    detail_section(ui, |ui| {
        info_row(ui, "处理器名称", or_dash(&m.cpu_model));
        ui.add_space(6.0);
        section_row(ui, "处理器参数", |ui| {
            table_header(ui, &["核心数", "频率", "缓存", "BogoMips"]);
            table_row(
                ui,
                &[
                    num_or_dash(m.cpu_cores),
                    if m.cpu_mhz > 0.0 {
                        format!("{:.3}", m.cpu_mhz)
                    } else {
                        "—".into()
                    },
                    if m.cpu_cache_kb > 0 {
                        fmt_kb(m.cpu_cache_kb)
                    } else {
                        "—".into()
                    },
                    if m.cpu_bogomips > 0.0 {
                        format!("{:.2}", m.cpu_bogomips)
                    } else {
                        "—".into()
                    },
                ],
            );
        });
        ui.add_space(8.0);

        section_row(ui, "占用分类", |ui| {
            table_header(
                ui,
                &[
                    "用户",
                    "系统",
                    "Nice",
                    "空闲",
                    "IO",
                    "硬件中断",
                    "软件中断",
                    "实时",
                ],
            );
            match &m.cpu_busy {
                Some(b) => table_row(
                    ui,
                    &[
                        pct1(b.user),
                        pct1(b.system),
                        pct1(b.nice),
                        pct1(b.idle),
                        pct1(b.iowait),
                        pct1(b.irq),
                        pct1(b.softirq),
                        pct1(b.steal),
                    ],
                ),
                // 本机(sysinfo)拿不到分项
                None => table_row(ui, &["—", "—", "—", "—", "—", "—", "—", "—"]),
            }
        });
    });
    ui.add_space(10.0);

    // ---- 内存 / 交换 ----
    detail_section(ui, |ui| {
        let min_block_width = 310.0;
        let two_columns =
            ui.available_width() >= min_block_width * 2.0 + ui.spacing().item_spacing.x;
        if two_columns {
            let divider_x = ui.max_rect().center().x;
            let divider_top = ui.next_widget_position().y;
            ui.columns(2, |columns| {
                memory_block(&mut columns[0], "物理内存", m.mem_total_kb, m.mem_used_kb);
                memory_block(&mut columns[1], "交换空间", m.swap_total_kb, m.swap_used_kb);
            });
            let divider_bottom = ui.next_widget_position().y;
            ui.painter().vline(
                divider_x,
                divider_top..=divider_bottom,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(218, 222, 228)),
            );
        } else {
            memory_block(ui, "物理内存", m.mem_total_kb, m.mem_used_kb);
            ui.add_space(8.0);
            memory_block(ui, "交换空间", m.swap_total_kb, m.swap_used_kb);
        }
    });
    ui.add_space(10.0);

    // ---- 网络接口 ----
    detail_section(ui, |ui| {
        section_row(ui, "接口", |ui| {
            table_header(ui, &["名称", "发送", "接收", "发送速度", "接收速度"]);
            for n in &m.nets {
                table_row(
                    ui,
                    &[
                        n.name.clone(),
                        fmt_bytes(n.tx_bytes),
                        fmt_bytes(n.rx_bytes),
                        format!("{}/s", fmt_rate(n.tx_bps)),
                        format!("{}/s", fmt_rate(n.rx_bps)),
                    ],
                );
            }
            if m.nets.is_empty() {
                table_row(ui, &["—", "", "", "", ""]);
            }
        });
    });
    ui.add_space(6.0);
    ui.separator();
    ui.add_space(6.0);

    // ---- 文件系统(全部,含 tmpfs 等虚拟文件系统) ----
    section_row(ui, "文件系统", |ui| {
        table_header(ui, &["名称", "大小", "已用", "", "可用", "挂载点"]);
        for d in &m.disks {
            table_row(
                ui,
                &[
                    d.fs.clone(),
                    fmt_kb(d.total_kb),
                    fmt_kb(d.used_kb),
                    format!("{:.1}%", d.usage_frac() * 100.0),
                    fmt_kb(d.avail_kb),
                    d.path.clone(),
                ],
            );
        }
        if m.disks.is_empty() {
            table_row(ui, &["—", "", "", "", "", ""]);
        }
    });
    ui.add_space(10.0);
}

/// 行首标签 + 内容的通用行;内容区是竖向布局(表头在上、数据行在下)
fn section_row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_top(|ui| {
        ui.add_sized([LABEL_W, 18.0], egui::Label::new(label));
        ui.vertical(add);
    });
}

/// 带边框的独立信息区块，用于清晰区分不同类别。
fn detail_section(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(250, 251, 253))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgb(218, 222, 228),
        ))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn info_row(ui: &mut egui::Ui, label: &str, value: &str) {
    section_row(ui, label, |ui| {
        ui.label(value);
    });
}

/// 表头:弱化文本,定宽列(第一列更宽)
fn table_header(ui: &mut egui::Ui, cols: &[&str]) {
    ui.horizontal(|ui| {
        for (i, c) in cols.iter().enumerate() {
            let w = if i == 0 { 200.0 } else { 90.0 };
            ui.add_sized([w, 16.0], egui::Label::new(RichText::new(*c).weak()));
        }
    });
}

fn table_row<S: AsRef<str>>(ui: &mut egui::Ui, cols: &[S]) {
    ui.horizontal(|ui| {
        for (i, c) in cols.iter().enumerate() {
            let w = if i == 0 { 200.0 } else { 90.0 };
            ui.add_sized([w, 16.0], egui::Label::new(c.as_ref()).truncate());
        }
    });
}

fn or_dash(s: &str) -> &str {
    if s.is_empty() { "—" } else { s }
}

fn num_or_dash(n: u32) -> String {
    if n == 0 { "—".into() } else { n.to_string() }
}

fn pct1(v: f32) -> String {
    if v <= 0.0 {
        "0%".into()
    } else {
        format!("{v:.1}%")
    }
}

fn memory_block(ui: &mut egui::Ui, name: &str, total_kb: u64, used_kb: u64) {
    memory_columns(ui, &[name, "总量", "已使用", "占用率", "剩余"], true);

    if total_kb == 0 {
        memory_columns(ui, &["", "—", "—", "—", "—"], false);
        return;
    }

    let used_kb = used_kb.min(total_kb);
    let percent = used_kb as f64 / total_kb as f64 * 100.0;
    memory_columns(
        ui,
        &[
            String::new(),
            fmt_kb(total_kb),
            fmt_kb(used_kb),
            format!("{percent:.0}%"),
            fmt_kb(total_kb - used_kb),
        ],
        false,
    );
}

fn memory_columns<S: AsRef<str>>(ui: &mut egui::Ui, values: &[S; 5], header: bool) {
    let spacing = ui.spacing().item_spacing.x;
    let name_width = 68.0;
    let value_width = ((ui.available_width() - name_width - spacing * 4.0) / 4.0).max(48.0);
    ui.horizontal(|ui| {
        for (index, value) in values.iter().enumerate() {
            let text = if header && index > 0 {
                RichText::new(value.as_ref()).weak()
            } else if header {
                RichText::new(value.as_ref()).strong()
            } else {
                RichText::new(value.as_ref())
            };
            let width = if index == 0 { name_width } else { value_width };
            ui.add_sized([width, 16.0], egui::Label::new(text));
        }
    });
}
