// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! 配置类文件的安全读写工具。
//!
//! 所有写入都先落到同目录临时文件，完成同步后再原子替换正式文件。
//! 解析失败的文件会保留为带时间戳的 `.corrupt-*` 副本，避免后续保存
//! 默认值时永久覆盖唯一可恢复的数据。

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn backup_corrupt(path: &Path) -> anyhow::Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let backup = path.with_file_name(format!("{file_name}.corrupt-{stamp}"));
    if let Err(rename_error) = fs::rename(path, &backup) {
        fs::copy(path, &backup).map_err(|copy_error| {
            anyhow::anyhow!("移动损坏文件失败：{rename_error}；复制备份也失败：{copy_error}")
        })?;
    }
    Ok(backup)
}

pub fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("目标文件没有父目录：{}", path.display()))?;
    fs::create_dir_all(parent)?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let temp = parent.join(format!(".{file_name}.{stamp}.tmp"));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_contents() {
        let dir = std::env::temp_dir().join(format!("rshell-storage-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("value.toml");
        atomic_write(&path, b"one", true).unwrap();
        atomic_write(&path, b"two", true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_backup_preserves_bytes_and_removes_source() {
        let dir = std::env::temp_dir().join(format!("rshell-corrupt-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.toml");
        fs::write(&path, b"not = [valid").unwrap();
        let backup = backup_corrupt(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(fs::read(backup).unwrap(), b"not = [valid");
        let _ = fs::remove_dir_all(dir);
    }
}
