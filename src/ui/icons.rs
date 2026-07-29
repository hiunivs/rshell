//! RShell 的统一 Lucide SVG 图标。
//!
//! SVG 字节编译进程序；egui 的加载器会按稳定 URI 缓存解析和纹理结果，
//! `tint` 只在绘制时着色，因此明暗主题切换不会重复解析资源。

use egui::{Color32, CursorIcon, Image, Rect, Response, Sense, Ui, Vec2};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Icon {
    BookOpen,
    ChevronDown,
    ChevronRight,
    Copy,
    Download,
    File,
    Folder,
    KeyRound,
    Pause,
    Pencil,
    Play,
    Plus,
    Power,
    RefreshCw,
    Settings,
    SquareTerminal,
    Trash,
    Upload,
    X,
}

impl Icon {
    fn resource(self) -> (&'static str, &'static [u8]) {
        match self {
            Self::BookOpen => (
                "bytes://rshell-icons/lucide/book-open.svg",
                include_bytes!("../../assets/icons/lucide/book-open.svg"),
            ),
            Self::ChevronDown => (
                "bytes://rshell-icons/lucide/chevron-down.svg",
                include_bytes!("../../assets/icons/lucide/chevron-down.svg"),
            ),
            Self::ChevronRight => (
                "bytes://rshell-icons/lucide/chevron-right.svg",
                include_bytes!("../../assets/icons/lucide/chevron-right.svg"),
            ),
            Self::Copy => (
                "bytes://rshell-icons/lucide/copy.svg",
                include_bytes!("../../assets/icons/lucide/copy.svg"),
            ),
            Self::Download => (
                "bytes://rshell-icons/lucide/download.svg",
                include_bytes!("../../assets/icons/lucide/download.svg"),
            ),
            Self::File => (
                "bytes://rshell-icons/lucide/file.svg",
                include_bytes!("../../assets/icons/lucide/file.svg"),
            ),
            Self::Folder => (
                "bytes://rshell-icons/lucide/folder.svg",
                include_bytes!("../../assets/icons/lucide/folder.svg"),
            ),
            Self::KeyRound => (
                "bytes://rshell-icons/lucide/key-round.svg",
                include_bytes!("../../assets/icons/lucide/key-round.svg"),
            ),
            Self::Pause => (
                "bytes://rshell-icons/lucide/pause.svg",
                include_bytes!("../../assets/icons/lucide/pause.svg"),
            ),
            Self::Pencil => (
                "bytes://rshell-icons/lucide/pencil.svg",
                include_bytes!("../../assets/icons/lucide/pencil.svg"),
            ),
            Self::Plus => (
                "bytes://rshell-icons/lucide/plus.svg",
                include_bytes!("../../assets/icons/lucide/plus.svg"),
            ),
            Self::Play => (
                "bytes://rshell-icons/lucide/play.svg",
                include_bytes!("../../assets/icons/lucide/play.svg"),
            ),
            Self::Power => (
                "bytes://rshell-icons/lucide/power.svg",
                include_bytes!("../../assets/icons/lucide/power.svg"),
            ),
            Self::RefreshCw => (
                "bytes://rshell-icons/lucide/refresh-cw.svg",
                include_bytes!("../../assets/icons/lucide/refresh-cw.svg"),
            ),
            Self::Settings => (
                "bytes://rshell-icons/lucide/settings.svg",
                include_bytes!("../../assets/icons/lucide/settings.svg"),
            ),
            Self::SquareTerminal => (
                "bytes://rshell-icons/lucide/square-terminal.svg",
                include_bytes!("../../assets/icons/lucide/square-terminal.svg"),
            ),
            Self::Trash => (
                "bytes://rshell-icons/lucide/trash-2.svg",
                include_bytes!("../../assets/icons/lucide/trash-2.svg"),
            ),
            Self::Upload => (
                "bytes://rshell-icons/lucide/upload.svg",
                include_bytes!("../../assets/icons/lucide/upload.svg"),
            ),
            Self::X => (
                "bytes://rshell-icons/lucide/x.svg",
                include_bytes!("../../assets/icons/lucide/x.svg"),
            ),
        }
    }

    pub(crate) fn image(self, size: f32, color: Color32) -> Image<'static> {
        let (uri, bytes) = self.resource();
        Image::from_bytes(uri, bytes)
            .fit_to_exact_size(Vec2::splat(size))
            .tint(color)
    }

    pub(crate) fn paint_at(self, ui: &Ui, rect: Rect, color: Color32) {
        self.image(rect.width().min(rect.height()), color)
            .paint_at(ui, rect);
    }
}

/// 无常驻背景的紧凑图标按钮。悬停背景画在固定分配矩形内，不改变布局尺寸。
pub(crate) fn icon_button(
    ui: &mut Ui,
    icon: Icon,
    tip: &str,
    button_size: Vec2,
    icon_size: f32,
    color: Color32,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(button_size, Sense::click());
    let response = response
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tip);
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 3.0, ui.visuals().widgets.hovered.bg_fill);
    }
    let paint_color = if response.hovered() {
        color.gamma_multiply(1.2)
    } else {
        color
    };
    icon.paint_at(
        ui,
        Rect::from_center_size(rect.center(), Vec2::splat(icon_size)),
        paint_color,
    );
    response
}
