// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

use super::{UpdateAction, UpdateBackend};
use crate::config::UpdateChannel;

pub struct UnsupportedBackend;

impl UpdateBackend for UnsupportedBackend {
    fn platform_label(&self) -> &'static str {
        "当前平台"
    }

    fn availability(&self, _channel: UpdateChannel) -> Result<(), String> {
        Err("当前平台暂不支持应用内更新".to_owned())
    }

    fn check_for_updates(&mut self, _channel: UpdateChannel) -> Result<UpdateAction, String> {
        Err("当前平台暂不支持应用内更新".to_owned())
    }
}
