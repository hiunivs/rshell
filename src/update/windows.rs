// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

use super::{UpdateAction, UpdateBackend, secure_update_url};
use crate::config::UpdateChannel;

pub struct WindowsBackend;

impl WindowsBackend {
    pub fn new() -> Self {
        Self
    }

    fn installer_url(channel: UpdateChannel) -> Result<String, String> {
        let value = match channel {
            UpdateChannel::Stable => option_env!("RSHELL_WINDOWS_APPINSTALLER_URL"),
            UpdateChannel::Beta => option_env!("RSHELL_WINDOWS_BETA_APPINSTALLER_URL"),
        }
        .ok_or_else(|| match channel {
            UpdateChannel::Stable => "此构建未配置 Windows 更新地址".to_owned(),
            UpdateChannel::Beta => "此构建未配置测试版更新源".to_owned(),
        })?;
        secure_update_url(value)
    }
}

impl UpdateBackend for WindowsBackend {
    fn platform_label(&self) -> &'static str {
        "Windows · MSIX App Installer"
    }

    fn availability(&self, channel: UpdateChannel) -> Result<(), String> {
        Self::installer_url(channel).map(|_| ())
    }

    fn check_for_updates(&mut self, channel: UpdateChannel) -> Result<UpdateAction, String> {
        Ok(UpdateAction::OpenUrl(Self::installer_url(channel)?))
    }
}
