//! 连接与界面配置的读写：使用平台标准 App 配置目录。
//! 密码不写入此文件，由 secrets 模块按安全设置保存。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

static CONFIG_WRITABLE: AtomicBool = AtomicBool::new(true);
static LOAD_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn record_load_error(message: String) {
    if let Ok(mut error) = LOAD_ERROR.lock() {
        *error = Some(message);
    }
}

pub fn take_load_error() -> Option<String> {
    LOAD_ERROR.lock().ok()?.take()
}

pub const DEFAULT_TERMINAL_BACKGROUND_COLOR: [u8; 3] = [0x1B, 0x2B, 0x3A];
pub const TERMINAL_BACKGROUND_PRESETS: [[u8; 3]; 3] = [
    DEFAULT_TERMINAL_BACKGROUND_COLOR,
    [0x17, 0x19, 0x1C],
    [0x24, 0x25, 0x29],
];

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PasswordStorage {
    SystemKeyring,
    #[default]
    AppEncrypted,
}

impl PasswordStorage {
    pub fn label(self) -> &'static str {
        match self {
            Self::SystemKeyring => "系统钥匙串",
            Self::AppEncrypted => "App 内加密",
        }
    }

    pub fn remember_password_label(self) -> &'static str {
        match self {
            Self::SystemKeyring => "记住密码（系统钥匙串）",
            Self::AppEncrypted => "记住密码",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    System,
    Dark,
    Light,
}

impl ThemeMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "系统",
            Self::Dark => "深色",
            Self::Light => "浅色",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ColorScheme {
    #[default]
    Blue,
    Green,
    Purple,
    Orange,
}

impl ColorScheme {
    pub fn label(self) -> &'static str {
        match self {
            Self::Blue => "蓝色",
            Self::Green => "绿色",
            Self::Purple => "紫色",
            Self::Orange => "橙色",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalBackgroundKind {
    #[default]
    Solid,
    Image,
}

impl TerminalBackgroundKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Solid => "纯色",
            Self::Image => "图片",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalImageMode {
    #[default]
    Fill,
    Fit,
    Stretch,
    Center,
    Tile,
}

impl TerminalImageMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fill => "填充",
            Self::Fit => "适应",
            Self::Stretch => "拉伸",
            Self::Center => "居中",
            Self::Tile => "平铺",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct TerminalBackground {
    pub kind: TerminalBackgroundKind,
    pub color: [u8; 3],
    pub image_path: String,
    pub image_mode: TerminalImageMode,
    /// 覆盖在图片上的黑色遮罩强度，范围 0..=100。
    pub image_darkness: u8,
}

impl Default for TerminalBackground {
    fn default() -> Self {
        Self {
            kind: TerminalBackgroundKind::Solid,
            color: DEFAULT_TERMINAL_BACKGROUND_COLOR,
            image_path: String::new(),
            image_mode: TerminalImageMode::Fill,
            image_darkness: 45,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SshTerminalSettings {
    pub font_size: u8,
}

impl Default for SshTerminalSettings {
    fn default() -> Self {
        Self { font_size: 13 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ShortcutSettings {
    /// 在终端输出区域单击鼠标右键时请求系统剪贴板内容并粘贴。
    pub right_click_paste: bool,
}

/// 一个已保存的连接。`name`(=user@host:port) 仍是稳定主键。
/// 新增字段全部带默认值,旧 toml 缺字段时 serde 自动填默认(向后兼容)。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SavedConnection {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// 私钥路径;为空表示密码认证(当 key_id 为 None 时生效)
    pub key_path: String,
    /// 显示名；空则回退 name
    pub alias: String,
    /// 所在文件夹路径("/" 分隔);空 = 根/未分组
    pub group: String,
    /// 标签
    pub tags: Vec<String>,
    /// 指向密钥库(SavedKey.id);Some 时连接用该托管密钥,覆盖 key_path/密码
    pub key_id: Option<String>,
}

impl SavedConnection {
    /// 列表/树里显示的标题
    pub fn display_name(&self) -> &str {
        if self.alias.is_empty() {
            &self.name
        } else {
            &self.alias
        }
    }

    /// SSH 标签标题：优先使用显示名，否则使用主机地址。
    pub fn session_title(&self) -> String {
        let target = if self.alias.is_empty() {
            &self.host
        } else {
            &self.alias
        };
        format!("{}@{target}", self.user)
    }
}

/// 一把受管理的密钥(生成或登记)。私钥文件实际在 `path`。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SavedKey {
    pub id: String,
    pub name: String,
    pub path: String,
    /// sha256 指纹(展示用;空则在加载/登记时补算)
    pub fingerprint: String,
}

/// 用户添加的常用命令。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SavedCommand {
    pub name: String,
    pub command: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct AppConfig {
    pub connections: Vec<SavedConnection>,
    /// 显式存在的文件夹路径集合(含空文件夹)
    pub folders: Vec<String>,
    /// 受管理的密钥
    pub keys: Vec<SavedKey>,
    /// 用户自定义的常用命令；内置命令不写入配置。
    pub common_commands: Vec<SavedCommand>,
    /// 密码和私钥口令的保存后端
    pub password_storage: PasswordStorage,
    /// 主题颜色模式
    pub theme_mode: ThemeMode,
    /// 界面强调色
    pub color_scheme: ColorScheme,
    /// 终端输出区域的背景。
    pub terminal_background: TerminalBackground,
    /// SSH/本机终端的显示设置。
    pub ssh_terminal: SshTerminalSettings,
    /// 鼠标与键盘快捷操作。
    pub shortcuts: ShortcutSettings,
    /// 上次退出时主窗口内容区域的宽度和高度。
    pub window_width: Option<f32>,
    pub window_height: Option<f32>,
}

impl AppConfig {
    pub fn window_size(&self) -> Option<[f32; 2]> {
        let width = self.window_width?;
        let height = self.window_height?;
        if width.is_finite() && height.is_finite() && width >= 1024.0 && height >= 640.0 {
            Some([width, height])
        } else {
            None
        }
    }
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "rshell", "rshell")
}

fn config_path() -> Option<PathBuf> {
    project_dirs().map(|p| p.config_dir().join("connections.toml"))
}

/// 受管理密钥的存放目录(<config>/keys)
pub fn keys_dir() -> Option<PathBuf> {
    project_dirs().map(|p| p.config_dir().join("keys"))
}

pub fn config_dir() -> Option<PathBuf> {
    project_dirs().map(|p| p.config_dir().to_path_buf())
}

pub fn load() -> AppConfig {
    let Some(path) = config_path() else {
        return AppConfig::default();
    };
    let text = match crate::storage::read_optional(&path) {
        Ok(Some(text)) => text,
        Ok(None) => return AppConfig::default(),
        Err(error) => {
            CONFIG_WRITABLE.store(false, Ordering::Relaxed);
            record_load_error(format!("读取配置失败，本次运行不会覆盖原文件：{error:#}"));
            eprintln!("读取配置失败（不会覆盖原文件）: {error:#}");
            return AppConfig::default();
        }
    };
    match toml::from_str(&text) {
        Ok(config) => config,
        Err(error) => {
            match crate::storage::backup_corrupt(&path) {
                Ok(backup) => {
                    let message = format!(
                        "配置文件格式错误；原文件已备份到 {}：{error}",
                        backup.display()
                    );
                    record_load_error(message.clone());
                    eprintln!("{message}");
                }
                Err(backup_error) => {
                    CONFIG_WRITABLE.store(false, Ordering::Relaxed);
                    let message = format!(
                        "配置文件格式错误且备份失败，本次运行不会保存配置：{error}；{backup_error:#}"
                    );
                    record_load_error(message.clone());
                    eprintln!("{message}");
                }
            }
            AppConfig::default()
        }
    }
}

pub fn save(cfg: &AppConfig) -> anyhow::Result<()> {
    (|| {
        if !CONFIG_WRITABLE.load(Ordering::Relaxed) {
            anyhow::bail!("启动时读取配置失败，本次运行已禁止覆盖配置文件");
        }
        let Some(path) = config_path() else {
            anyhow::bail!("无法确定配置目录");
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(cfg)?;
        crate::storage::atomic_write(&path, text.as_bytes(), true)?;
        Ok(())
    })()
    .inspect_err(|error| eprintln!("保存配置失败：{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_roundtrip() {
        let mut cfg = AppConfig::default();
        cfg.connections.push(SavedConnection {
            name: "root@example.com".into(),
            host: "example.com".into(),
            port: 22,
            user: "root".into(),
            key_path: "~/.ssh/id_rsa".into(),
            ..Default::default()
        });
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.connections.len(), 1);
        assert_eq!(parsed.connections[0].host, "example.com");
        assert_eq!(parsed.connections[0].port, 22);
    }

    /// 旧格式 toml(只有旧连接字段,无 alias/group/tags/key_id,无 folders/keys)
    /// 必须能加载,缺字段取默认
    #[test]
    fn load_legacy_toml() {
        let legacy = r#"
[[connections]]
name = "root@1.2.3.4:22"
host = "1.2.3.4"
port = 22
user = "root"
key_path = ""
"#;
        let cfg: AppConfig = toml::from_str(legacy).unwrap();
        assert_eq!(cfg.connections.len(), 1);
        let c = &cfg.connections[0];
        assert_eq!(c.host, "1.2.3.4");
        assert!(c.alias.is_empty());
        assert!(c.group.is_empty());
        assert!(c.tags.is_empty());
        assert!(c.key_id.is_none());
        assert!(cfg.folders.is_empty());
        assert!(cfg.keys.is_empty());
        assert!(cfg.common_commands.is_empty());
        assert!(cfg.window_size().is_none());
        assert_eq!(cfg.password_storage, PasswordStorage::AppEncrypted);
        assert_eq!(cfg.terminal_background.kind, TerminalBackgroundKind::Solid);
        assert_eq!(cfg.terminal_background.color, [0x1B, 0x2B, 0x3A]);
        assert_eq!(cfg.terminal_background.image_darkness, 45);
        assert_eq!(cfg.ssh_terminal.font_size, 13);
        assert!(!cfg.shortcuts.right_click_paste);
    }

    /// 新字段往返
    #[test]
    fn new_fields_roundtrip() {
        let mut cfg = AppConfig::default();
        cfg.folders.push("生产".into());
        cfg.keys.push(SavedKey {
            id: "k1".into(),
            name: "work".into(),
            path: "/x/k1".into(),
            fingerprint: "SHA256:abc".into(),
        });
        cfg.password_storage = PasswordStorage::AppEncrypted;
        cfg.theme_mode = ThemeMode::Dark;
        cfg.color_scheme = ColorScheme::Purple;
        cfg.terminal_background.kind = TerminalBackgroundKind::Image;
        cfg.terminal_background.image_path = "/x/background.png".into();
        cfg.terminal_background.image_mode = TerminalImageMode::Fit;
        cfg.terminal_background.image_darkness = 60;
        cfg.ssh_terminal.font_size = 18;
        cfg.shortcuts.right_click_paste = true;
        cfg.window_width = Some(1360.0);
        cfg.window_height = Some(860.0);
        cfg.connections.push(SavedConnection {
            name: "root@h:22".into(),
            host: "h".into(),
            port: 22,
            user: "root".into(),
            alias: "我的服务器".into(),
            group: "生产".into(),
            tags: vec!["web".into(), "prod".into()],
            key_id: Some("k1".into()),
            ..Default::default()
        });
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.folders, vec!["生产".to_string()]);
        assert_eq!(parsed.keys[0].name, "work");
        assert_eq!(parsed.password_storage, PasswordStorage::AppEncrypted);
        assert_eq!(parsed.theme_mode, ThemeMode::Dark);
        assert_eq!(parsed.color_scheme, ColorScheme::Purple);
        assert_eq!(
            parsed.terminal_background.kind,
            TerminalBackgroundKind::Image
        );
        assert_eq!(parsed.terminal_background.image_path, "/x/background.png");
        assert_eq!(
            parsed.terminal_background.image_mode,
            TerminalImageMode::Fit
        );
        assert_eq!(parsed.terminal_background.image_darkness, 60);
        assert_eq!(parsed.ssh_terminal.font_size, 18);
        assert!(parsed.shortcuts.right_click_paste);
        assert_eq!(parsed.window_size(), Some([1360.0, 860.0]));
        let c = &parsed.connections[0];
        assert_eq!(c.alias, "我的服务器");
        assert_eq!(c.group, "生产");
        assert_eq!(c.tags, vec!["web".to_string(), "prod".to_string()]);
        assert_eq!(c.key_id.as_deref(), Some("k1"));
        assert_eq!(c.display_name(), "我的服务器");
        assert_eq!(c.session_title(), "root@我的服务器");
    }

    #[test]
    fn session_title_falls_back_to_host() {
        let connection = SavedConnection {
            host: "10.0.0.8".into(),
            user: "admin".into(),
            ..Default::default()
        };
        assert_eq!(connection.session_title(), "admin@10.0.0.8");
    }
}
