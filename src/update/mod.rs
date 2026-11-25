// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! 安全的软件更新入口。
//!
//! 这里不下载、解压或覆盖当前可执行文件。macOS 将安装过程交给 Sparkle，
//! Windows 将安装过程交给系统 App Installer。

use crate::config::{UpdateChannel, UpdateSettings};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

/// 平台后端需要 UI 执行的动作。
#[allow(dead_code)] // 每个平台只构造其中一种动作。
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateAction {
    /// 平台原生更新窗口已经启动。
    Started,
    /// 交给操作系统打开已固定的 HTTPS App Installer 地址。
    OpenUrl(String),
}

/// 平台后端的最小接口。安装动作必须留在平台受信任的更新器中。
pub trait UpdateBackend {
    fn platform_label(&self) -> &'static str;
    fn availability(&self, channel: UpdateChannel) -> Result<(), String>;
    fn check_for_updates(&mut self, channel: UpdateChannel) -> Result<UpdateAction, String>;

    fn configure_channel(&mut self, channel: UpdateChannel) -> Result<(), String> {
        self.availability(channel)
    }

    fn start(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn manages_automatic_checks(&self) -> bool {
        false
    }

    fn set_automatic_checks(&mut self, _enabled: bool) -> Result<(), String> {
        Ok(())
    }
}

/// 应用持有的统一更新状态。
pub struct UpdateManager {
    backend: Box<dyn UpdateBackend>,
    status: String,
    started: bool,
    automatic_checks: bool,
}

impl UpdateManager {
    pub fn new(settings: &UpdateSettings) -> Self {
        let mut manager = Self {
            backend: platform_backend(),
            status: String::new(),
            started: false,
            automatic_checks: settings.automatic_checks,
        };
        manager.status = match manager.activate(settings.channel) {
            Ok(()) => String::new(),
            Err(error) => error,
        };
        manager
    }

    pub fn platform_label(&self) -> &'static str {
        self.backend.platform_label()
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn is_available(&self, channel: UpdateChannel) -> bool {
        self.backend.availability(channel).is_ok()
    }

    pub fn manages_automatic_checks(&self) -> bool {
        self.backend.manages_automatic_checks()
    }

    pub fn configure_channel(&mut self, channel: UpdateChannel) -> bool {
        match self.activate(channel) {
            Ok(()) => {
                self.status = "更新通道已切换".to_owned();
                true
            }
            Err(error) => {
                // 不可用通道不能继续沿用上一个有效 feed 做自动检查。
                if self.backend.manages_automatic_checks() {
                    let _ = self.backend.set_automatic_checks(false);
                }
                self.automatic_checks = false;
                self.status = error;
                false
            }
        }
    }

    pub fn set_automatic_checks(&mut self, enabled: bool) {
        self.automatic_checks = enabled;
        self.status = match self.backend.set_automatic_checks(enabled) {
            Ok(()) if enabled => "已开启自动检查".to_owned(),
            Ok(()) => "已关闭自动检查".to_owned(),
            Err(error) => error,
        };
    }

    pub fn check_for_updates(&mut self, channel: UpdateChannel) -> Result<UpdateAction, String> {
        if !self.started {
            self.activate(channel)?;
        }
        match self.backend.check_for_updates(channel) {
            Ok(action) => {
                self.status = match action {
                    // Sparkle 自带原生进度 UI，这里不再显示冗余的检查中小字。
                    UpdateAction::Started => String::new(),
                    UpdateAction::OpenUrl(_) => "已打开 Windows 更新下载地址".to_owned(),
                };
                Ok(action)
            }
            Err(error) => {
                self.status.clone_from(&error);
                Err(error)
            }
        }
    }

    fn activate(&mut self, channel: UpdateChannel) -> Result<(), String> {
        self.backend.configure_channel(channel)?;
        if self.backend.manages_automatic_checks() {
            self.backend.set_automatic_checks(self.automatic_checks)?;
        }
        if !self.started {
            self.backend.start()?;
            self.started = true;
        }
        Ok(())
    }
}

fn platform_backend() -> Box<dyn UpdateBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacOsBackend::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsBackend::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Box::new(unsupported::UnsupportedBackend)
    }
}

pub(crate) fn secure_update_url(value: &str) -> Result<String, String> {
    let parsed = url::Url::parse(value).map_err(|_| "更新地址格式无效".to_owned())?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err("更新地址必须使用 HTTPS".to_owned());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("更新地址不能包含用户名或密码".to_owned());
    }
    if parsed.fragment().is_some() {
        return Err("更新地址不能包含片段标识".to_owned());
    }
    Ok(parsed.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_urls_must_be_plain_https_urls() {
        assert!(secure_update_url("https://updates.example.com/stable/feed.xml").is_ok());
        assert!(secure_update_url("http://updates.example.com/feed.xml").is_err());
        assert!(secure_update_url("https://user:pass@example.com/feed.xml").is_err());
        assert!(secure_update_url("https://example.com/feed.xml#old").is_err());
    }
}
