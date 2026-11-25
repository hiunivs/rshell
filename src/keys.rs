// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! 受管理的 SSH 密钥:生成 / 登记 / 删除 / 连接时解析。
//! 生成或登记的私钥文件实际路径记在 `SavedKey.path`。
//! 生成的密钥写入 `<config>/keys/<id>`(unix 0600)+ 同名 `.pub`。
//! 口令不进本模块:加密落盘时由调用方传入,连接解密口令由 UI 存 keyring
//! (账号 `key:<id>`),故本模块不依赖 keyring,便于纯文件单测。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};

use crate::config::{AppConfig, SavedKey};

/// 生成密钥时可选的算法
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeyAlgo {
    Ed25519,
    Rsa,
}

impl KeyAlgo {
    fn to_algorithm(self) -> Algorithm {
        match self {
            KeyAlgo::Ed25519 => Algorithm::Ed25519,
            KeyAlgo::Rsa => Algorithm::Rsa { hash: None },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            KeyAlgo::Ed25519 => "Ed25519",
            KeyAlgo::Rsa => "RSA",
        }
    }
}

const LE: russh::keys::ssh_key::LineEnding = russh::keys::ssh_key::LineEnding::LF;

fn key_err<E: std::fmt::Display>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// keyring 里存放某把密钥口令的账号名
pub fn key_account(id: &str) -> String {
    format!("key:{id}")
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| format!("{h}/{rest}"))
            .unwrap_or_else(|_| path.to_string())
    } else {
        path.to_string()
    }
}

fn new_id() -> String {
    static C: AtomicU64 = AtomicU64::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:016x}{:08x}{:04x}", t, n, std::process::id() as u16)
}

/// 公钥 sha256 指纹(展示用)
pub fn fingerprint_of(pk: &PublicKey) -> String {
    pk.fingerprint(HashAlg::Sha256).to_string()
}

/// 确保密钥目录存在(unix 0700)
pub fn ensure_keys_dir() -> Result<PathBuf> {
    let d = crate::config::keys_dir().context("无法确定配置目录")?;
    std::fs::create_dir_all(&d).with_context(|| format!("创建密钥目录失败: {}", d.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    }
    Ok(d)
}

/// 生成新密钥对(写入 app 配置目录的 keys/)
pub fn generate(name: &str, algo: KeyAlgo, passphrase: Option<&str>) -> Result<SavedKey> {
    let dir = ensure_keys_dir()?;
    generate_into(&dir, name, algo, passphrase)
}

/// 生成到指定目录(供测试用临时目录)
pub fn generate_into(
    dir: &Path,
    name: &str,
    algo: KeyAlgo,
    passphrase: Option<&str>,
) -> Result<SavedKey> {
    std::fs::create_dir_all(dir).with_context(|| format!("创建目录失败: {}", dir.display()))?;
    let mut rng = rand::rng();
    let key = PrivateKey::random(&mut rng, algo.to_algorithm()).map_err(key_err)?;
    let fp = fingerprint_of(key.public_key());
    let pub_line = key.public_key().to_openssh().map_err(key_err)?;
    let pem = match passphrase {
        Some(p) if !p.is_empty() => key
            .encrypt(&mut rng, p.as_bytes())
            .map_err(key_err)?
            .to_openssh(LE)
            .map_err(key_err)?,
        _ => key.to_openssh(LE).map_err(key_err)?,
    };
    let id = new_id();
    let priv_path = dir.join(&id);
    let pub_path = dir.join(format!("{id}.pub"));
    crate::storage::atomic_write(&priv_path, pem.as_bytes(), true)
        .with_context(|| format!("写私钥失败: {}", priv_path.display()))?;
    crate::storage::atomic_write(&pub_path, pub_line.as_bytes(), false)
        .with_context(|| format!("写公钥失败: {}", pub_path.display()))?;
    Ok(SavedKey {
        id,
        name: name.to_string(),
        path: priv_path.to_string_lossy().into_owned(),
        fingerprint: fp,
    })
}

/// 登记一把已有私钥(校验可读 + 算指纹;口令由调用方存 keyring)
pub fn register(name: &str, path: &str, passphrase: Option<&str>) -> Result<SavedKey> {
    let exp = expand_tilde(path);
    let pass = passphrase.filter(|p| !p.is_empty());
    let key = russh::keys::load_secret_key(&exp, pass).map_err(key_err)?;
    let fp = fingerprint_of(key.public_key());
    Ok(SavedKey {
        id: new_id(),
        name: name.to_string(),
        path: path.to_string(),
        fingerprint: fp,
    })
}

/// 删除:移除条目 + 受管文件 + 清空引用(口令由调用方清 keyring)
pub fn delete(id: &str, cfg: &mut AppConfig) {
    cfg.keys.retain(|k| k.id != id);
    if let Some(dir) = crate::config::keys_dir() {
        let p = dir.join(id);
        if p.exists() {
            let _ = std::fs::remove_file(&p);
        }
        let pp = dir.join(format!("{id}.pub"));
        if pp.exists() {
            let _ = std::fs::remove_file(&pp);
        }
    }
    for c in cfg.connections.iter_mut() {
        if c.key_id.as_deref() == Some(id) {
            c.key_id = None;
        }
    }
}

/// 改名
pub fn rename(id: &str, new_name: &str, cfg: &mut AppConfig) -> Result<()> {
    let k = cfg
        .keys
        .iter_mut()
        .find(|k| k.id == id)
        .context("密钥不存在")?;
    k.name = new_name.to_string();
    Ok(())
}

/// 改指向的私钥文件(重新校验 + 重算指纹)
pub fn repoint(
    id: &str,
    new_path: &str,
    passphrase: Option<&str>,
    cfg: &mut AppConfig,
) -> Result<()> {
    let exp = expand_tilde(new_path);
    let pass = passphrase.filter(|p| !p.is_empty());
    let key = russh::keys::load_secret_key(&exp, pass).map_err(key_err)?;
    let fp = fingerprint_of(key.public_key());
    let k = cfg
        .keys
        .iter_mut()
        .find(|k| k.id == id)
        .context("密钥不存在")?;
    k.path = new_path.to_string();
    k.fingerprint = fp;
    Ok(())
}

/// 连接时解析:(展开后的路径, keyring 中的口令)
pub fn load_for_connect(
    key_id: &str,
    cfg: &AppConfig,
) -> Option<(String, Option<crate::secrets::SecretString>)> {
    let k = cfg.keys.iter().find(|k| k.id == key_id)?;
    let path = expand_tilde(&k.path);
    let pass = crate::secrets::get(&key_account(&k.id));
    Some((path, pass))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录生成 ed25519 → 文件存在 → 读回成功 → 指纹非空(不碰 keyring)
    #[test]
    fn generate_and_loadback() {
        let dir = std::env::temp_dir().join(format!("rshell_keys_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let k = generate_into(&dir, "test-key", KeyAlgo::Ed25519, None).unwrap();
        assert!(!k.id.is_empty());
        assert!(!k.fingerprint.is_empty());
        assert!(Path::new(&k.path).exists(), "私钥文件应存在");
        assert!(
            Path::new(&format!("{}.pub", k.path)).exists(),
            "公钥文件应存在"
        );
        // 读回(无口令)
        let loaded = russh::keys::load_secret_key(&k.path, None).unwrap();
        assert_eq!(fingerprint_of(loaded.public_key()), k.fingerprint);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 加密落盘:带口令生成 → 无口令读回失败 → 带口令读回成功
    #[test]
    fn generate_encrypted() {
        let dir = std::env::temp_dir().join(format!("rshell_keys_enc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let k = generate_into(&dir, "enc", KeyAlgo::Ed25519, Some("hunter2")).unwrap();
        assert!(
            russh::keys::load_secret_key(&k.path, None).is_err(),
            "无口令应解密失败"
        );
        let loaded = russh::keys::load_secret_key(&k.path, Some("hunter2")).unwrap();
        assert_eq!(fingerprint_of(loaded.public_key()), k.fingerprint);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 登记:用生成的文件作为"已有密钥"登记
    #[test]
    fn register_existing() {
        let dir = std::env::temp_dir().join(format!("rshell_keys_reg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let gk = generate_into(&dir, "g", KeyAlgo::Ed25519, None).unwrap();
        let reg = register("my-alias", &gk.path, None).unwrap();
        assert_eq!(reg.fingerprint, gk.fingerprint);
        assert_eq!(reg.path, gk.path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// delete 清空连接引用 + 删受管文件
    #[test]
    fn delete_clears_refs() {
        let dir = std::env::temp_dir().join(format!("rshell_keys_del_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let k = generate_into(&dir, "d", KeyAlgo::Ed25519, None).unwrap();
        let mut cfg = AppConfig::default();
        cfg.keys.push(k.clone());
        cfg.connections.push(crate::config::SavedConnection {
            name: "u@h:22".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            key_id: Some(k.id.clone()),
            ..Default::default()
        });
        // 让 keys_dir 指向临时目录不可行(它读真实配置目录),
        // 故此处只验证 cfg 引用被清空 + 条目移除;文件删除依赖真实 keys_dir,手测覆盖。
        delete(&k.id, &mut cfg);
        assert!(cfg.keys.is_empty());
        assert!(cfg.connections[0].key_id.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
