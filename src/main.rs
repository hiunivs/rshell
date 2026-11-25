// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

mod app;
mod app_render_test;
mod config;
mod export;
mod file_kind;
mod file_manager;
mod keys;
mod known_hosts;
#[cfg(target_os = "macos")]
mod macos_dock;
mod panel_drag_test;

mod secrets;
mod session;
mod storage;
mod term_view;
mod ui;
mod update;

/// macOS/Metal 在长时间休眠/后台后可能拿不到新的 drawable；wgpu 把它映射为
/// `CurrentSurfaceTexture::Timeout`。egui-wgpu 默认对 Timeout 直接跳帧，
/// 会导致窗口一直停在旧画面/花屏。这里改为先强制重配 surface，连续多次
/// 超时后重建 surface。
fn wgpu_options() -> eframe::WgpuConfiguration {
    use std::sync::atomic::{AtomicU32, Ordering};
    static TIMEOUT_STREAK: AtomicU32 = AtomicU32::new(0);

    let mut config = eframe::WgpuConfiguration::default();
    let fallback = config.on_surface_status.clone();
    config.on_surface_status = std::sync::Arc::new(move |status| match status {
        eframe::wgpu::CurrentSurfaceTexture::Timeout => {
            let streak = TIMEOUT_STREAK.fetch_add(1, Ordering::Relaxed) + 1;
            if streak >= 3 {
                TIMEOUT_STREAK.store(0, Ordering::Relaxed);
                eframe::egui_wgpu::SurfaceErrorAction::RecreateSurface
            } else {
                eframe::egui_wgpu::SurfaceErrorAction::Reconfigure
            }
        }
        other => {
            TIMEOUT_STREAK.store(0, Ordering::Relaxed);
            fallback(other)
        }
    });
    config
}

/// 强制下一帧重建字体图集并整张上传。
///
/// 窗口不可见期间 egui 的字体图集如果发生重建，纹理增量可能没有被上传；
/// 重新激活后 CPU 侧 UV 与 GPU 纹理不一致，会让所有文字和纯色图形看起来
/// “重影/花屏”。这里通过一次无视觉影响的 FontDefinitions 变化触发完整图集重建。
pub(crate) fn force_font_texture_reupload(ctx: &egui::Context) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TOGGLE: AtomicBool = AtomicBool::new(false);

    let mut definitions = ctx.fonts(|fonts| fonts.definitions().clone());
    let Some(first) = definitions.font_data.keys().next().cloned() else {
        return;
    };
    let second = definitions
        .font_data
        .keys()
        .nth(1)
        .cloned()
        .unwrap_or_else(|| first.clone());
    let refresh_family = egui::FontFamily::Name("rshell_texture_refresh".into());
    // 没有任何控件使用这个字体族，只用来让 FontDefinitions 与上次不同。
    let chain = if TOGGLE.fetch_xor(true, Ordering::Relaxed) {
        vec![first]
    } else {
        vec![first, second]
    };
    definitions.families.insert(refresh_family, chain);
    ctx.set_fonts(definitions);
    ctx.request_repaint();
}

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
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    eframe::run_native(
        "RShell",
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            // debug 构建下 egui 默认开启两项调试绘制,都会产生"红框"假象:
            // - show_unaligned:小数坐标的 Ui 矩形画橙框+"Unaligned"字样
            // - warn_if_rect_changes_id:同一矩形上的控件 id 逐帧变化时画纯红 2px 框
            //   (egui_extras 表格单元格的 unique_id 含创建序号,可见行范围一挪就全变,
            //   拖分隔条/滚动时正是这样,纯属误报)
            // Style.debug 仅在 debug_assertions 开启时存在,release 构建必须跳过。
            #[cfg(debug_assertions)]
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

/// 安装内置 UI 字体和用户选择的终端等宽字体。
/// 结果全局缓存(OnceLock),字体字节只保留一份并存活到进程退出。
pub(crate) fn install_app_fonts(ctx: &egui::Context, terminal_font_family: &str) {
    let resources =
        FONT_RESOURCES.get_or_init(|| build_font_resources(terminal_font_family.trim()));
    ctx.set_fonts(resources.definitions.clone());
}

/// 设置页可选择的系统等宽字体。字体数据库在启动安装字体时已完成扫描，
/// 打开设置页不会再次扫描磁盘。
pub(crate) fn terminal_font_families() -> &'static [String] {
    FONT_RESOURCES
        .get()
        .map(|resources| resources.terminal_families.as_slice())
        .unwrap_or(&[])
}

const TERMINAL_FONT_FAMILY: &str = "rshell_terminal";
const UI_FONT_NAME: &str = "rshell_ui";
const UI_FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/RShellUI-Regular.otf");

/// 终端及内建编辑器共用的字体 ID，确保两处始终使用相同主字体和回退链。
pub(crate) fn terminal_font_id(size: f32) -> egui::FontId {
    static FAMILY: std::sync::OnceLock<egui::FontFamily> = std::sync::OnceLock::new();
    let family = FAMILY
        .get_or_init(|| egui::FontFamily::Name(TERMINAL_FONT_FAMILY.into()))
        .clone();
    egui::FontId::new(size.clamp(8.0, 32.0), family)
}

struct FontResources {
    definitions: egui::FontDefinitions,
    terminal_families: Vec<String>,
}

static FONT_RESOURCES: std::sync::OnceLock<FontResources> = std::sync::OnceLock::new();

fn build_font_resources(terminal_font_family: &str) -> FontResources {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let mut terminal_families = std::collections::BTreeSet::new();
    for face in db
        .faces()
        .filter(|face| face.monospaced && face.style == fontdb::Style::Normal)
    {
        if let Some((family, _)) = face.families.first() {
            terminal_families.insert(family.clone());
        }
    }
    terminal_families.remove("Hack");
    let terminal_families = std::iter::once("Hack".to_owned())
        .chain(terminal_families)
        .collect::<Vec<_>>();

    let mut fonts = egui::FontDefinitions::default();
    // 内置 Noto Sans SC 同时覆盖中英文，使用字体原始度量。不要再叠加过去
    // 为系统字体回退差异准备的缩放或基线偏移，否则会让字形变小、视觉上移。
    let ui_font = egui::FontData::from_static(UI_FONT_BYTES);
    fonts
        .font_data
        .insert(UI_FONT_NAME.to_owned(), std::sync::Arc::new(ui_font));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        // UI 中英文和常用符号统一使用内置字体，不再受操作系统字体影响。
        .insert(0, UI_FONT_NAME.to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        // 终端保持等宽主字体，只把内置字体作为 CJK 回退。
        .push(UI_FONT_NAME.to_owned());

    // 终端使用独立命名字体族，不改变普通 UI 和内建编辑器的字体。
    // 只加载用户当前选中的一个系统等宽字体；不存在或不是等宽字体时回退 Hack。
    let mut terminal_primary = "Hack".to_owned();
    if !terminal_font_family.is_empty() && terminal_font_family != "Hack" {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(terminal_font_family)],
            ..Default::default()
        };
        if let Some(id) = db.query(&query)
            && db.face(id).is_some_and(|face| face.monospaced)
            && let Some((source, index)) = db.face_source(id)
            && let Some(data) = load_font_data(&source, index)
        {
            fonts
                .font_data
                .insert("terminal_custom".to_owned(), std::sync::Arc::new(data));
            terminal_primary = "terminal_custom".to_owned();
        }
    }

    let mut terminal_chain = vec![terminal_primary];
    terminal_chain.push(UI_FONT_NAME.to_owned());
    fonts.families.insert(
        egui::FontFamily::Name(TERMINAL_FONT_FAMILY.into()),
        terminal_chain,
    );

    FontResources {
        definitions: fonts,
        terminal_families,
    }
}

/// egui 0.35 会在构造字体 Blob 时克隆 owned 字节。将系统字体固定为进程级
/// borrowed 数据，可避免大型 TTC 在 `FontDefinitions` 和字体 Blob 中各保留一份。
///
/// 此函数只会在全局 `FONT_RESOURCES` 首次初始化时调用，泄漏的缓冲区数量固定，并在
/// 进程退出时由操作系统回收；它不会随窗口或 viewport 数量增长。
fn load_font_data(source: &fontdb::Source, index: u32) -> Option<egui::FontData> {
    let bytes = match source {
        fontdb::Source::File(path) => std::fs::read(path).ok()?,
        fontdb::Source::Binary(data) => data.as_ref().as_ref().to_vec(),
        fontdb::Source::SharedFile(_, data) => data.as_ref().as_ref().to_vec(),
    };
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let mut data = egui::FontData::from_static(bytes);
    data.index = index;
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui_wgpu::SurfaceErrorAction;
    use eframe::wgpu::CurrentSurfaceTexture;

    #[test]
    fn surface_timeout_reconfigures_then_recreates() {
        let config = wgpu_options();
        assert!(matches!(
            (config.on_surface_status)(&CurrentSurfaceTexture::Timeout),
            SurfaceErrorAction::Reconfigure
        ));
        assert!(matches!(
            (config.on_surface_status)(&CurrentSurfaceTexture::Timeout),
            SurfaceErrorAction::Reconfigure
        ));
        assert!(matches!(
            (config.on_surface_status)(&CurrentSurfaceTexture::Timeout),
            SurfaceErrorAction::RecreateSurface
        ));
        assert!(matches!(
            (config.on_surface_status)(&CurrentSurfaceTexture::Occluded),
            SurfaceErrorAction::SkipFrame
        ));
    }

    #[test]
    fn forced_font_reupload_changes_font_definitions() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(Default::default(), |_ui| {});
        force_font_texture_reupload(&ctx);
        let _ = ctx.run_ui(Default::default(), |_ui| {});
        let definitions = ctx.fonts(|fonts| fonts.definitions().clone());
        assert!(
            definitions
                .families
                .contains_key(&egui::FontFamily::Name("rshell_texture_refresh".into())),
            "强制重建字体图集应改变 FontDefinitions"
        );
    }
}
