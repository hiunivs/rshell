mod app;
mod app_render_test;
mod config;
mod export;
mod file_manager;
mod keys;
mod known_hosts;
mod panel_drag_test;

mod secrets;
mod session;
mod storage;
mod term_view;
mod ui;

fn main() -> eframe::Result<()> {
    let loaded_config = config::load();
    let initial_window_size = loaded_config.window_size().unwrap_or([1280.0, 800.0]);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RShell")
            .with_app_id("com.rshell.rshell")
            .with_inner_size(initial_window_size)
            .with_min_inner_size([1024.0, 640.0])
            // macOS 原生标题栏使用透明材质，由 egui 背景提供一致的颜色。
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(true)
            .with_titlebar_buttons_shown(true),
        ..Default::default()
    };
    eframe::run_native(
        "RShell",
        options,
        Box::new(move |cc| {
            // debug 构建下 egui 默认开启两项调试绘制,都会产生"红框"假象:
            // - show_unaligned:小数坐标的 Ui 矩形画橙框+"Unaligned"字样
            // - warn_if_rect_changes_id:同一矩形上的控件 id 逐帧变化时画纯红 2px 框
            //   (egui_extras 表格单元格的 unique_id 含创建序号,可见行范围一挪就全变,
            //   拖分隔条/滚动时正是这样,纯属误报)
            cc.egui_ctx.all_styles_mut(|s| {
                s.debug.show_unaligned = false;
                s.debug.warn_if_rect_changes_id = false;
            });
            // RSHELL_DEBUG_SHAPES=1:注册诊断插件,在每帧最终输出里找偏红描边
            // (on_output 在 end_pass 之后,能看到 egui 自己追加的所有调试描边)
            if std::env::var_os("RSHELL_DEBUG_SHAPES").is_some() {
                cc.egui_ctx.add_plugin(RedFramePlugin::default());
            }
            Ok(Box::new(app::RShellApp::new_with_config(cc, loaded_config)))
        }),
    )
}

/// 诊断插件:打印每帧输出中偏红/橙色的描边形状(矩形/线/路径/圆),按形状去重
#[derive(Default)]
struct RedFramePlugin(std::sync::Mutex<std::collections::HashSet<String>>);

impl RedFramePlugin {
    fn check(
        layer: &str,
        clip: egui::Rect,
        shape: &egui::Shape,
        seen: &mut std::collections::HashSet<String>,
    ) {
        let reddish = |c: egui::Color32| {
            c.r() as i16 > c.g() as i16 + 30 && c.r() as i16 > c.b() as i16 + 30 && c.r() > 120
        };
        let (kind, rect, color) = match shape {
            egui::Shape::Rect(r) => ("Rect", r.rect, r.stroke.color),
            egui::Shape::LineSegment { points, stroke } => (
                "Line",
                egui::Rect::from_two_pos(points[0], points[1]),
                stroke.color,
            ),
            egui::Shape::Path(p) => (
                "Path",
                p.visual_bounding_rect(),
                match p.stroke.color {
                    epaint::ColorMode::Solid(c) => c,
                    _ => egui::Color32::TRANSPARENT,
                },
            ),
            egui::Shape::Circle(c) => (
                "Circle",
                egui::Rect::from_center_size(c.center, egui::vec2(c.radius * 2.0, c.radius * 2.0)),
                c.stroke.color,
            ),
            egui::Shape::Vec(v) => {
                for s in v {
                    Self::check(layer, clip, s, seen);
                }
                return;
            }
            _ => return,
        };
        if reddish(color) {
            let key = format!(
                "{kind}:({:.0},{:.0})-({:.0},{:.0})",
                rect.left(),
                rect.top(),
                rect.right(),
                rect.bottom()
            );
            if seen.insert(key) {
                eprintln!(
                    "[redframe] layer={layer} kind={kind} rect={rect:?} color={color:?} clip={clip:?}"
                );
            }
        }
    }
}

impl egui::Plugin for RedFramePlugin {
    fn debug_name(&self) -> &'static str {
        "RedFramePlugin"
    }

    fn output_hook(&mut self, _ctx: &egui::Context, output: &mut egui::FullOutput) {
        let mut seen = self.0.lock().unwrap();
        for cs in &output.shapes {
            Self::check("output", cs.clip_rect, &cs.shape, &mut seen);
        }
    }
}

/// 加载系统 CJK 字体作为回退,保证中文 UI 与终端中文输出正常显示。
pub(crate) fn install_cjk_fonts(ctx: &egui::Context) {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    // 按平台常见程度排序的候选中文字体
    const CANDIDATES: &[&str] = &[
        "PingFang SC",         // macOS
        "Hiragino Sans GB",    // macOS 备选
        "Microsoft YaHei",     // Windows
        "Noto Sans CJK SC",    // Linux
        "WenQuanYi Micro Hei", // Linux 备选
        "SimHei",              // Windows 备选
    ];

    let mut fonts = egui::FontDefinitions::default();
    let mut loaded = false;
    for family in CANDIDATES {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            ..Default::default()
        };
        let Some(id) = db.query(&query) else {
            continue;
        };
        let Some((source, index)) = db.face_source(id) else {
            continue;
        };
        let bytes = match &source {
            fontdb::Source::File(path) => std::fs::read(path).ok(),
            fontdb::Source::Binary(data) => Some(data.as_ref().as_ref().to_vec()),
            fontdb::Source::SharedFile(_, data) => Some(data.as_ref().as_ref().to_vec()),
        };
        let Some(bytes) = bytes else { continue };

        let mut data = egui::FontData::from_owned(bytes);
        data.index = index;
        fonts
            .font_data
            .insert("cjk".to_owned(), std::sync::Arc::new(data));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push("cjk".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".to_owned());
        loaded = true;
        break;
    }

    // UI 符号字体回退。默认字体与 CJK 字体都不保证覆盖 ⌂、三角等符号。
    const SYMBOL_CANDIDATES: &[&str] = &[
        "Apple Symbols",       // macOS
        "Segoe UI Symbol",     // Windows
        "Noto Sans Symbols 2", // Linux
        "DejaVu Sans",         // Linux 备选
        "Arial Unicode MS",    // macOS 备选
    ];
    for family in SYMBOL_CANDIDATES {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            ..Default::default()
        };
        let Some(id) = db.query(&query) else {
            continue;
        };
        let Some((source, index)) = db.face_source(id) else {
            continue;
        };
        let bytes = match &source {
            fontdb::Source::File(path) => std::fs::read(path).ok(),
            fontdb::Source::Binary(data) => Some(data.as_ref().as_ref().to_vec()),
            fontdb::Source::SharedFile(_, data) => Some(data.as_ref().as_ref().to_vec()),
        };
        let Some(bytes) = bytes else { continue };

        let mut data = egui::FontData::from_owned(bytes);
        data.index = index;
        fonts
            .font_data
            .insert("symbols".to_owned(), std::sync::Arc::new(data));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push("symbols".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("symbols".to_owned());
        loaded = true;
        break;
    }

    if loaded {
        ctx.set_fonts(fonts);
    }
}
