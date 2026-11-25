// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! 密码安全存储：可选择系统 keyring，或使用 AES-256-GCM 加密保存在 App 配置目录。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use aes_gcm::aead::{Aead, Generate, Key, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::config::PasswordStorage;

const SERVICE: &str = "rshell";
const KEYRING_BACKEND: u8 = 0;
const APP_BACKEND: u8 = 1;
static BACKEND: AtomicU8 = AtomicU8::new(KEYRING_BACKEND);

/// 进程内共享的明文秘密。克隆只增加引用计数，最后一个引用释放时
/// `Zeroizing` 会清空底层字符串，避免 SSH 重连和独立传输连接制造多份长期副本。
#[derive(Clone, Default)]
pub struct SecretString(Arc<Zeroizing<String>>);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Arc::new(Zeroizing::new(value.into())))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

#[derive(Serialize, Deserialize, Default)]
struct EncryptedStore {
    entries: HashMap<String, String>,
}

/// keyring 账号名：与已保存连接一一对应。
pub fn account_for(user: &str, host: &str, port: u16) -> String {
    format!("{user}@{host}:{port}")
}

pub fn configure(storage: PasswordStorage) {
    BACKEND.store(storage_code(storage), Ordering::Relaxed);
}

pub fn set(account: &str, password: &str) -> Result<(), String> {
    let storage = current_storage();
    set_in(storage, account, password).inspect_err(|error| eprintln!("保存密码失败：{error}"))?;
    delete_from(other_storage(storage), account);
    Ok(())
}

pub fn get(account: &str) -> Option<SecretString> {
    let storage = current_storage();
    get_from(storage, account).or_else(|| get_from(other_storage(storage), account))
}

/// 删除账号时同时清理两种后端，避免切换方式后遗留旧密码。
pub fn delete(account: &str) {
    delete_from(PasswordStorage::SystemKeyring, account);
    delete_from(PasswordStorage::AppEncrypted, account);
}

/// 切换保存后端，并迁移当前配置中已知账号的密码/私钥口令。
pub fn switch_backend(
    from: PasswordStorage,
    to: PasswordStorage,
    accounts: &[String],
) -> Result<(), String> {
    if from == to {
        configure(to);
        return Ok(());
    }
    let mut values = Vec::new();
    for account in accounts {
        if let Some(password) = get_from(from, account) {
            values.push((account.clone(), password));
        }
    }
    for (account, password) in &values {
        set_in(to, account, password.expose())?;
    }
    for (account, _) in &values {
        delete_from(from, account);
    }
    configure(to);
    Ok(())
}

fn storage_code(storage: PasswordStorage) -> u8 {
    match storage {
        PasswordStorage::SystemKeyring => KEYRING_BACKEND,
        PasswordStorage::AppEncrypted => APP_BACKEND,
    }
}

fn current_storage() -> PasswordStorage {
    match BACKEND.load(Ordering::Relaxed) {
        APP_BACKEND => PasswordStorage::AppEncrypted,
        _ => PasswordStorage::SystemKeyring,
    }
}

fn other_storage(storage: PasswordStorage) -> PasswordStorage {
    match storage {
        PasswordStorage::SystemKeyring => PasswordStorage::AppEncrypted,
        PasswordStorage::AppEncrypted => PasswordStorage::SystemKeyring,
    }
}

fn set_in(storage: PasswordStorage, account: &str, password: &str) -> Result<(), String> {
    match storage {
        PasswordStorage::SystemKeyring => keyring_set(account, password),
        PasswordStorage::AppEncrypted => app_set(account, password),
    }
}

fn get_from(storage: PasswordStorage, account: &str) -> Option<SecretString> {
    match storage {
        PasswordStorage::SystemKeyring => keyring_get(account),
        PasswordStorage::AppEncrypted => app_get(account),
    }
}

fn delete_from(storage: PasswordStorage, account: &str) {
    match storage {
        PasswordStorage::SystemKeyring => keyring_delete(account),
        PasswordStorage::AppEncrypted => app_delete(account),
    }
}

fn keyring_set(account: &str, password: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, account).map_err(|e| e.to_string())?;
    entry.set_password(password).map_err(|e| e.to_string())
}

fn keyring_get(account: &str) -> Option<SecretString> {
    keyring::Entry::new(SERVICE, account)
        .ok()?
        .get_password()
        .ok()
        .map(SecretString::from)
}

fn keyring_delete(account: &str) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, account) {
        let _ = entry.delete_credential();
    }
}

fn app_paths() -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let dir = crate::config::config_dir().ok_or_else(|| "无法确定 App 配置目录".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    Ok((dir.join("secrets.key"), dir.join("secrets.toml")))
}

fn load_or_create_key() -> Result<[u8; 32], String> {
    let (key_path, store_path) = app_paths()?;
    match std::fs::read(&key_path) {
        Ok(bytes) => {
            return <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
                let backup = crate::storage::backup_corrupt(&key_path)
                    .map(|path| format!("；原文件已备份到 {}", path.display()))
                    .unwrap_or_else(|error| format!("；备份原文件失败：{error:#}"));
                format!("加密密钥文件长度错误{backup}，已停止写入以保护现有密码")
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("读取加密密钥失败：{error}")),
    }
    if store_path.exists() && std::fs::metadata(&store_path).map(|m| m.len()).unwrap_or(0) > 0 {
        return Err(format!(
            "密码文件存在但加密密钥缺失，已停止生成新密钥：{}",
            key_path.display()
        ));
    }
    let generated = Key::<Aes256Gcm>::generate();
    let key: [u8; 32] = generated
        .as_slice()
        .try_into()
        .map_err(|_| "生成加密密钥失败".to_string())?;
    crate::storage::atomic_write(&key_path, &key, true)
        .map_err(|e| format!("写入加密密钥失败：{e:#}"))?;
    Ok(key)
}

fn load_store() -> Result<EncryptedStore, String> {
    let (_, store_path) = app_paths()?;
    let text = match crate::storage::read_optional(&store_path) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(EncryptedStore::default()),
        Err(error) => return Err(format!("读取密码文件失败：{error:#}")),
    };
    toml::from_str(&text).map_err(|error| match crate::storage::backup_corrupt(&store_path) {
        Ok(backup) => format!(
            "密码文件格式错误：{error}；原文件已备份到 {}，已停止写入",
            backup.display()
        ),
        Err(backup_error) => {
            format!("密码文件格式错误：{error}；备份失败：{backup_error:#}，已停止写入")
        }
    })
}

fn save_store(store: &EncryptedStore) -> Result<(), String> {
    let (_, store_path) = app_paths()?;
    let text = toml::to_string_pretty(store).map_err(|e| format!("编码密码文件失败：{e}"))?;
    crate::storage::atomic_write(&store_path, text.as_bytes(), true)
        .map_err(|e| format!("写入密码文件失败：{e:#}"))?;
    Ok(())
}

fn app_set(account: &str, password: &str) -> Result<(), String> {
    let mut store = load_store()?;
    let key = load_or_create_key()?;
    let encrypted = encrypt_value(&key, account, password)?;
    store.entries.insert(account.to_string(), encrypted);
    save_store(&store)
}

fn app_get(account: &str) -> Option<SecretString> {
    let store = load_store()
        .inspect_err(|error| eprintln!("{error}"))
        .ok()?;
    let encoded = store.entries.get(account)?;
    let key = load_or_create_key().ok()?;
    decrypt_value(&key, account, encoded)
        .ok()
        .map(SecretString::from)
}

fn app_delete(account: &str) {
    let Ok(mut store) = load_store().inspect_err(|error| eprintln!("{error}")) else {
        return;
    };
    if store.entries.remove(account).is_some()
        && let Err(error) = save_store(&store)
    {
        eprintln!("{error}");
    }
}

fn encrypt_value(key: &[u8; 32], account: &str, password: &str) -> Result<String, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "初始化加密器失败".to_string())?;
    let nonce = Nonce::generate();
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: password.as_bytes(),
                aad: account.as_bytes(),
            },
        )
        .map_err(|_| "加密密码失败".to_string())?;
    let mut value = nonce.to_vec();
    value.extend_from_slice(&ciphertext);
    Ok(base64::engine::general_purpose::STANDARD_NO_PAD.encode(value))
}

fn decrypt_value(key: &[u8; 32], account: &str, encoded: &str) -> Result<String, String> {
    let value = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(encoded)
        .map_err(|_| "密码数据格式错误".to_string())?;
    if value.len() <= 12 {
        return Err("密码数据不完整".into());
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "初始化解密器失败".to_string())?;
    let nonce = Nonce::try_from(&value[..12]).map_err(|_| "密码 nonce 长度错误".to_string())?;
    let plaintext = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &value[12..],
                aad: account.as_bytes(),
            },
        )
        .map_err(|_| "解密密码失败".to_string())?;
    String::from_utf8(plaintext).map_err(|_| "密码不是有效 UTF-8".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_value_roundtrip_and_account_binding() {
        let key = [7u8; 32];
        let encoded = encrypt_value(&key, "root@example:22", "s3cret-测试").unwrap();
        assert!(!encoded.contains("s3cret"));
        assert_eq!(
            decrypt_value(&key, "root@example:22", &encoded).unwrap(),
            "s3cret-测试"
        );
        assert!(decrypt_value(&key, "other@example:22", &encoded).is_err());
    }

    /// 真实 keyring 读写（默认跳过：会写入系统钥匙串）。
    #[test]
    #[ignore]
    fn keyring_roundtrip() {
        let account = "rshell_test@127.0.0.1:22";
        keyring_delete(account);
        assert!(keyring_get(account).is_none());
        keyring_set(account, "s3cret-测试").unwrap();
        assert_eq!(
            keyring_get(account).as_ref().map(SecretString::expose),
            Some("s3cret-测试")
        );
        keyring_delete(account);
    }
}
