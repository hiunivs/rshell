//! 已知主机指纹库(TOFU):使用平台标准 App 配置目录。
//! 格式为 host:port → 指纹/算法/公钥文本,首次连接时由用户确认后写入。

use std::collections::HashMap;
use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct KnownHostKey {
    pub algorithm: String,
    /// "SHA256:xxxx" 形式
    pub fingerprint: String,
    /// "ssh-ed25519 AAAA..." 形式(完整公钥文本)
    pub openssh: String,
}

#[derive(Serialize, Deserialize)]
pub struct KnownHosts {
    #[serde(default)]
    pub hosts: HashMap<String, KnownHostKey>,
    #[serde(skip, default = "writable_default")]
    writable: bool,
}

fn writable_default() -> bool {
    true
}

impl Default for KnownHosts {
    fn default() -> Self {
        Self {
            hosts: HashMap::new(),
            writable: true,
        }
    }
}

/// 校验结果
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// 新主机,等待用户确认
    New,
    /// 指纹与记录一致
    Match,
    /// 指纹与记录不一致(可能被中间人攻击,也可能是服务器重装/换密钥)
    Changed { old_fingerprint: String },
}

fn default_path() -> Option<PathBuf> {
    ProjectDirs::from("com", "rshell", "rshell").map(|p| p.config_dir().join("known_hosts.toml"))
}

pub fn host_key_id(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

impl KnownHosts {
    pub fn load() -> Self {
        Self::load_from(default_path().as_deref())
    }

    fn load_from(path: Option<&std::path::Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        let text = match crate::storage::read_optional(path) {
            Ok(Some(text)) => text,
            Ok(None) => return Self::default(),
            Err(error) => {
                eprintln!("读取已知主机文件失败（不会覆盖原文件）：{error:#}");
                return Self {
                    writable: false,
                    ..Self::default()
                };
            }
        };
        match toml::from_str(&text) {
            Ok(hosts) => hosts,
            Err(error) => {
                match crate::storage::backup_corrupt(path) {
                    Ok(backup) => eprintln!(
                        "已知主机文件格式错误：{error}；原文件已备份到 {}",
                        backup.display()
                    ),
                    Err(backup_error) => {
                        eprintln!("已知主机文件格式错误：{error}；备份失败：{backup_error:#}");
                        return Self {
                            writable: false,
                            ..Self::default()
                        };
                    }
                }
                Self::default()
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        if !self.writable {
            anyhow::bail!("已知主机文件读取失败，本次连接已禁止覆盖该文件");
        }
        let Some(path) = default_path() else {
            anyhow::bail!("无法确定配置目录");
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)?;
        crate::storage::atomic_write(&path, text.as_bytes(), true)?;
        Ok(())
    }

    pub fn check(&self, host: &str, port: u16, fingerprint: &str) -> HostKeyStatus {
        let id = host_key_id(host, port);
        match self.hosts.get(&id) {
            None => HostKeyStatus::New,
            Some(k) if k.fingerprint == fingerprint => HostKeyStatus::Match,
            Some(k) => HostKeyStatus::Changed {
                old_fingerprint: k.fingerprint.clone(),
            },
        }
    }

    pub fn record(&mut self, host: &str, port: u16, key: KnownHostKey) {
        self.hosts.insert(host_key_id(host, port), key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(fp: &str) -> KnownHostKey {
        KnownHostKey {
            algorithm: "ssh-ed25519".into(),
            fingerprint: fp.into(),
            openssh: "ssh-ed25519 AAAA".into(),
        }
    }

    #[test]
    fn check_flow() {
        let mut kh = KnownHosts::default();
        assert_eq!(kh.check("h", 22, "SHA256:a"), HostKeyStatus::New);
        kh.record("h", 22, key("SHA256:a"));
        assert_eq!(kh.check("h", 22, "SHA256:a"), HostKeyStatus::Match);
        assert_eq!(
            kh.check("h", 22, "SHA256:b"),
            HostKeyStatus::Changed {
                old_fingerprint: "SHA256:a".into()
            }
        );
        // 端口不同视为不同主机
        assert_eq!(kh.check("h", 2222, "SHA256:a"), HostKeyStatus::New);
    }

    #[test]
    fn toml_roundtrip() {
        let mut kh = KnownHosts::default();
        kh.record("example.com", 22, key("SHA256:xyz"));
        let text = toml::to_string_pretty(&kh).unwrap();
        let parsed: KnownHosts = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed.check("example.com", 22, "SHA256:xyz"),
            HostKeyStatus::Match
        );
    }
}
