// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! SSH 连接 JSON 导出；支持明文和密码加密两种文件格式。

use aes_gcm::aead::{Aead, Generate, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, SavedConnection};

const FORMAT_NAME: &str = "rshell-ssh-connections";
const FORMAT_VERSION: u32 = 1;
const EXPORT_AAD: &[u8] = b"rshell-ssh-connections:v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionExportScope {
    All,
    Connection(String),
    Folder(String),
}

impl ConnectionExportScope {
    pub fn title(&self) -> String {
        match self {
            Self::All => "全部 SSH 连接".into(),
            Self::Connection(_) => "当前 SSH 连接".into(),
            Self::Folder(folder) => format!("文件夹“{}”", folder_name(folder)),
        }
    }

    pub fn default_file_name(&self) -> String {
        match self {
            Self::All => "rshell-connections.json".into(),
            Self::Connection(name) => format!("{}.json", safe_file_stem(name)),
            Self::Folder(folder) => {
                format!("rshell-{}.json", safe_file_stem(folder_name(folder)))
            }
        }
    }

    pub fn connections(&self, config: &AppConfig) -> Vec<SavedConnection> {
        match self {
            Self::All => config.connections.clone(),
            Self::Connection(name) => config
                .connections
                .iter()
                .filter(|connection| &connection.name == name)
                .cloned()
                .collect(),
            // 文件夹导出包含本文件夹及其全部子文件夹。
            Self::Folder(folder) => {
                let child_prefix = format!("{folder}/");
                config
                    .connections
                    .iter()
                    .filter(|connection| {
                        connection.group == *folder || connection.group.starts_with(&child_prefix)
                    })
                    .cloned()
                    .collect()
            }
        }
    }

    pub fn default_preserve_folders(&self) -> bool {
        !matches!(self, Self::Connection(_))
    }

    pub fn can_choose_preserve_folders(&self) -> bool {
        matches!(self, Self::All)
    }

    pub fn effective_preserve_folders(&self, requested: bool) -> bool {
        match self {
            Self::All => requested,
            Self::Connection(_) => false,
            Self::Folder(_) => true,
        }
    }

    pub fn folders(&self, config: &AppConfig, preserve: bool) -> Option<Vec<String>> {
        if !self.effective_preserve_folders(preserve) {
            return None;
        }
        let mut folders = match self {
            Self::All => config.folders.clone(),
            Self::Connection(_) => Vec::new(),
            Self::Folder(folder) => {
                let child_prefix = format!("{folder}/");
                let mut selected = config
                    .folders
                    .iter()
                    .filter(|candidate| {
                        *candidate == folder || candidate.starts_with(&child_prefix)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                // 兼容旧配置中仅由连接 group 隐式形成的目录。
                selected.push(folder.clone());
                selected
            }
        };
        folders.sort();
        folders.dedup();
        Some(folders)
    }

    pub fn include_connection_group(&self) -> bool {
        !matches!(self, Self::Connection(_))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportConnection {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub auth: ExportAuth,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExportAuth {
    Password {
        password: Option<String>,
    },
    PrivateKey {
        path: String,
    },
    ManagedKey {
        id: String,
        name: Option<String>,
        path: Option<String>,
        fingerprint: Option<String>,
        passphrase: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
struct ExportPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    folders: Option<Vec<String>>,
    connections: Vec<ExportConnection>,
}

#[derive(Serialize, Deserialize)]
struct PlainExportFile {
    format: String,
    version: u32,
    encrypted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    folders: Option<Vec<String>>,
    connections: Vec<ExportConnection>,
}

#[derive(Serialize, Deserialize)]
struct EncryptedExportFile {
    format: String,
    version: u32,
    encrypted: bool,
    encryption: EncryptionEnvelope,
}

#[derive(Serialize, Deserialize)]
struct EncryptionEnvelope {
    algorithm: String,
    kdf: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Clone, Debug)]
pub struct ImportPackage {
    pub folders: Option<Vec<String>>,
    pub connections: Vec<ExportConnection>,
}

#[derive(Clone, Debug)]
pub struct ConnectionImportState {
    pub path: std::path::PathBuf,
    pub json: String,
    pub encrypted: bool,
    pub password: String,
    pub error: Option<String>,
    pub plaintext_connection_count: Option<usize>,
}

impl ConnectionImportState {
    pub fn open(path: std::path::PathBuf) -> Result<Self, String> {
        let json =
            std::fs::read_to_string(&path).map_err(|error| format!("读取导入文件失败：{error}"))?;
        let encrypted = inspect_json(&json)?;
        let plaintext_connection_count = if encrypted {
            None
        } else {
            Some(decode_json(&json, None)?.connections.len())
        };
        Ok(Self {
            path,
            json,
            encrypted,
            password: String::new(),
            error: None,
            plaintext_connection_count,
        })
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("导入文件")
            .to_string()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub added: usize,
    pub merged: usize,
    pub folders_added: usize,
}

pub fn build_json(
    config: &AppConfig,
    connections: &[SavedConnection],
    folders: Option<Vec<String>>,
    include_connection_group: bool,
    password: Option<&str>,
) -> Result<String, String> {
    let exported = connections
        .iter()
        .map(|connection| export_connection(config, connection, include_connection_group))
        .collect::<Vec<_>>();

    match password {
        None => serde_json::to_string_pretty(&PlainExportFile {
            format: FORMAT_NAME.into(),
            version: FORMAT_VERSION,
            encrypted: false,
            folders,
            connections: exported,
        })
        .map_err(|error| format!("生成 JSON 失败：{error}")),
        Some(password) => {
            if password.is_empty() {
                return Err("加密密码不能为空".into());
            }
            let payload = serde_json::to_vec(&ExportPayload {
                folders,
                connections: exported,
            })
            .map_err(|error| format!("生成导出数据失败：{error}"))?;
            let encryption = encrypt_payload(password, &payload)?;
            serde_json::to_string_pretty(&EncryptedExportFile {
                format: FORMAT_NAME.into(),
                version: FORMAT_VERSION,
                encrypted: true,
                encryption,
            })
            .map_err(|error| format!("生成加密 JSON 失败：{error}"))
        }
    }
}

pub fn write_json(path: &std::path::Path, json: &str) -> Result<(), String> {
    crate::storage::atomic_write(path, json.as_bytes(), true)
        .map_err(|error| format!("写入导出文件失败：{error:#}"))
}

fn export_connection(
    config: &AppConfig,
    connection: &SavedConnection,
    include_group: bool,
) -> ExportConnection {
    let auth = if let Some(key_id) = &connection.key_id {
        let key = config.keys.iter().find(|key| &key.id == key_id);
        ExportAuth::ManagedKey {
            id: key_id.clone(),
            name: key.map(|key| key.name.clone()),
            path: key.map(|key| key.path.clone()),
            fingerprint: key.map(|key| key.fingerprint.clone()),
            passphrase: crate::secrets::get(&crate::keys::key_account(key_id))
                .map(|value| value.expose().to_owned()),
        }
    } else if connection.key_path.is_empty() {
        ExportAuth::Password {
            password: crate::secrets::get(&crate::secrets::account_for(
                &connection.user,
                &connection.host,
                connection.port,
            ))
            .map(|value| value.expose().to_owned()),
        }
    } else {
        ExportAuth::PrivateKey {
            path: connection.key_path.clone(),
        }
    };

    ExportConnection {
        name: connection.name.clone(),
        display_name: connection.display_name().to_string(),
        alias: connection.alias.clone(),
        host: connection.host.clone(),
        port: connection.port,
        user: connection.user.clone(),
        group: if include_group {
            connection.group.clone()
        } else {
            String::new()
        },
        tags: connection.tags.clone(),
        auth,
    }
}

fn encrypt_payload(password: &str, plaintext: &[u8]) -> Result<EncryptionEnvelope, String> {
    let mut salt = [0_u8; 16];
    rand::fill(&mut salt);
    let mut key = [0_u8; 32];
    argon2::Argon2::default()
        .hash_password_into(password.as_bytes(), &salt, &mut key)
        .map_err(|error| format!("派生加密密钥失败：{error}"))?;

    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "初始化加密器失败".to_string())?;
    let nonce = Nonce::generate();
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: EXPORT_AAD,
            },
        )
        .map_err(|_| "加密导出数据失败".to_string())?;
    key.fill(0);

    let encoder = base64::engine::general_purpose::STANDARD_NO_PAD;
    Ok(EncryptionEnvelope {
        algorithm: "AES-256-GCM".into(),
        kdf: "Argon2id".into(),
        salt: encoder.encode(salt),
        nonce: encoder.encode(nonce),
        ciphertext: encoder.encode(ciphertext),
    })
}

fn decrypt_payload(password: &str, envelope: &EncryptionEnvelope) -> Result<Vec<u8>, String> {
    let decoder = base64::engine::general_purpose::STANDARD_NO_PAD;
    let salt = decoder
        .decode(&envelope.salt)
        .map_err(|error| format!("盐值格式错误：{error}"))?;
    let nonce_bytes = decoder
        .decode(&envelope.nonce)
        .map_err(|error| format!("nonce 格式错误：{error}"))?;
    let ciphertext = decoder
        .decode(&envelope.ciphertext)
        .map_err(|error| format!("密文格式错误：{error}"))?;

    let mut key = [0_u8; 32];
    argon2::Argon2::default()
        .hash_password_into(password.as_bytes(), &salt, &mut key)
        .map_err(|error| format!("派生解密密钥失败：{error}"))?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "初始化解密器失败".to_string())?;
    let nonce =
        Nonce::try_from(nonce_bytes.as_slice()).map_err(|_| "nonce 长度错误".to_string())?;
    let plaintext = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &ciphertext,
                aad: EXPORT_AAD,
            },
        )
        .map_err(|_| "密码错误或密文已损坏".to_string())?;
    key.fill(0);
    Ok(plaintext)
}

pub fn inspect_json(json: &str) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct Header {
        format: String,
        version: u32,
        encrypted: bool,
    }

    let header: Header =
        serde_json::from_str(json).map_err(|error| format!("JSON 文件格式错误：{error}"))?;
    if header.format != FORMAT_NAME {
        return Err("不是 RShell SSH 连接导出文件".into());
    }
    if header.version != FORMAT_VERSION {
        return Err(format!("不支持的导出文件版本：{}", header.version));
    }
    Ok(header.encrypted)
}

pub fn decode_json(json: &str, password: Option<&str>) -> Result<ImportPackage, String> {
    if inspect_json(json)? {
        let file: EncryptedExportFile =
            serde_json::from_str(json).map_err(|error| format!("加密 JSON 格式错误：{error}"))?;
        if file.encryption.algorithm != "AES-256-GCM" || file.encryption.kdf != "Argon2id" {
            return Err("不支持的导出文件加密方式".into());
        }
        let password = password.ok_or_else(|| "请输入导出文件密码".to_string())?;
        if password.is_empty() {
            return Err("请输入导出文件密码".into());
        }
        let plaintext = decrypt_payload(password, &file.encryption)?;
        let payload: ExportPayload = serde_json::from_slice(&plaintext)
            .map_err(|error| format!("解密后的连接数据格式错误：{error}"))?;
        Ok(ImportPackage {
            folders: payload.folders,
            connections: payload.connections,
        })
    } else {
        let file: PlainExportFile =
            serde_json::from_str(json).map_err(|error| format!("连接 JSON 格式错误：{error}"))?;
        Ok(ImportPackage {
            folders: file.folders,
            connections: file.connections,
        })
    }
}

pub fn apply_import(
    config: &mut AppConfig,
    package: ImportPackage,
) -> Result<ImportSummary, String> {
    let folders_before = config
        .folders
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    if let Some(folders) = package.folders {
        for folder in folders {
            add_folder_with_ancestors(&mut config.folders, &folder);
        }
    }

    let mut summary = ImportSummary::default();
    for exported in package.connections {
        let host = exported.host.trim().to_string();
        let user = exported.user.trim().to_string();
        if host.is_empty() || user.is_empty() {
            return Err("导入文件中存在主机或用户名为空的连接".into());
        }
        if exported.port == 0 {
            return Err(format!("连接 {} 的端口无效", exported.display_name));
        }

        let group = normalize_folder_path(&exported.group);
        if !group.is_empty() {
            // 即使旧格式没有 folders，也可以从 group 恢复目录及祖先目录。
            add_folder_with_ancestors(&mut config.folders, &group);
        }
        let stable_name = crate::secrets::account_for(&user, &host, exported.port);
        let alias = if !exported.alias.trim().is_empty() {
            exported.alias.trim().to_string()
        } else if !exported.display_name.trim().is_empty() && exported.display_name != exported.name
        {
            exported.display_name.trim().to_string()
        } else {
            String::new()
        };

        let mut tags = exported
            .tags
            .into_iter()
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect::<Vec<_>>();
        tags.sort();
        tags.dedup();

        let (key_id, key_path) = match exported.auth {
            ExportAuth::Password { password } => {
                if let Some(password) = password {
                    crate::secrets::set(&stable_name, &password)
                        .map_err(|error| format!("导入连接密码失败：{error}"))?;
                }
                (None, String::new())
            }
            ExportAuth::PrivateKey { path } => {
                crate::secrets::delete(&stable_name);
                (None, path)
            }
            ExportAuth::ManagedKey {
                id,
                name,
                path,
                fingerprint,
                passphrase,
            } => {
                crate::secrets::delete(&stable_name);
                if let Some(existing) = config.keys.iter_mut().find(|key| key.id == id) {
                    if let Some(name) = name.filter(|value| !value.trim().is_empty()) {
                        existing.name = name;
                    }
                    if let Some(path) = path.filter(|value| !value.trim().is_empty()) {
                        existing.path = path;
                    }
                    if let Some(fingerprint) = fingerprint.filter(|value| !value.trim().is_empty())
                    {
                        existing.fingerprint = fingerprint;
                    }
                } else {
                    config.keys.push(crate::config::SavedKey {
                        id: id.clone(),
                        name: name
                            .filter(|value| !value.trim().is_empty())
                            .unwrap_or_else(|| id.clone()),
                        path: path.unwrap_or_default(),
                        fingerprint: fingerprint.unwrap_or_default(),
                    });
                }
                if let Some(passphrase) = passphrase {
                    crate::secrets::set(&crate::keys::key_account(&id), &passphrase)
                        .map_err(|error| format!("导入密钥口令失败：{error}"))?;
                }
                (Some(id), String::new())
            }
        };

        if let Some(existing) = config
            .connections
            .iter_mut()
            .find(|connection| connection.name == stable_name)
        {
            for tag in tags {
                if !existing.tags.contains(&tag) {
                    existing.tags.push(tag);
                }
            }
            existing.host = host;
            existing.port = exported.port;
            existing.user = user;
            existing.alias = alias;
            existing.group = group;
            existing.key_id = key_id;
            existing.key_path = key_path;
            summary.merged += 1;
        } else {
            config.connections.push(SavedConnection {
                name: stable_name,
                host,
                port: exported.port,
                user,
                key_path,
                alias,
                group,
                tags,
                key_id,
            });
            summary.added += 1;
        }
    }

    config.folders.sort();
    config.folders.dedup();
    summary.folders_added = config
        .folders
        .iter()
        .filter(|folder| !folders_before.contains(*folder))
        .count();
    Ok(summary)
}

fn normalize_folder_path(path: &str) -> String {
    path.split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect::<Vec<_>>()
        .join("/")
}

fn add_folder_with_ancestors(folders: &mut Vec<String>, path: &str) {
    let normalized = normalize_folder_path(path);
    let mut current = String::new();
    for part in normalized.split('/').filter(|part| !part.is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(part);
        if !folders.contains(&current) {
            folders.push(current.clone());
        }
    }
}

fn folder_name(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

fn safe_file_stem(value: &str) -> String {
    let safe = value
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            _ => character,
        })
        .collect::<String>();
    let safe = safe.trim().trim_matches('.').trim();
    if safe.is_empty() {
        "rshell-export".into()
    } else {
        safe.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (AppConfig, SavedConnection) {
        let connection = SavedConnection {
            name: "root@example.com:22".into(),
            alias: "生产服务器".into(),
            host: "example.com".into(),
            port: 22,
            user: "root".into(),
            group: "生产/Web".into(),
            tags: vec!["prod".into()],
            key_path: "~/.ssh/id_ed25519".into(),
            ..Default::default()
        };
        let mut config = AppConfig::default();
        config.connections.push(connection.clone());
        (config, connection)
    }

    #[test]
    fn folder_scope_includes_descendants_only() {
        let (mut config, connection) = fixture();
        config.connections.push(SavedConnection {
            name: "root@other:22".into(),
            group: "测试".into(),
            ..Default::default()
        });
        let selected = ConnectionExportScope::Folder("生产".into()).connections(&config);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, connection.name);
    }

    #[test]
    fn plaintext_json_exposes_connection_fields() {
        let (config, connection) = fixture();
        let json = build_json(&config, &[connection], None, true, None).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["encrypted"], false);
        assert_eq!(value["connections"][0]["host"], "example.com");
        assert_eq!(value["connections"][0]["display_name"], "生产服务器");
        assert_eq!(value["connections"][0]["group"], "生产/Web");
        assert!(value.get("folders").is_none());
    }

    #[test]
    fn encrypted_json_hides_connection_fields() {
        let (config, connection) = fixture();
        let json = build_json(
            &config,
            &[connection],
            Some(vec!["生产".into(), "生产/Web".into()]),
            true,
            Some("correct horse"),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["encrypted"], true);
        assert!(value.get("connections").is_none());
        assert!(value.get("folders").is_none());
        assert!(!json.contains("example.com"));
        assert!(!json.contains("生产/Web"));
        assert_eq!(value["encryption"]["algorithm"], "AES-256-GCM");
        assert_eq!(value["encryption"]["kdf"], "Argon2id");

        let file: EncryptedExportFile = serde_json::from_str(&json).unwrap();
        let plaintext = decrypt_payload("correct horse", &file.encryption).unwrap();
        let payload: ExportPayload = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(
            payload.folders,
            Some(vec!["生产".to_string(), "生产/Web".to_string()])
        );
        assert_eq!(payload.connections[0].host, "example.com");
        assert!(decrypt_payload("wrong password", &file.encryption).is_err());

        let imported = decode_json(&json, Some("correct horse")).unwrap();
        assert_eq!(imported.connections[0].host, "example.com");
        assert!(decode_json(&json, Some("wrong password")).is_err());
    }

    #[test]
    fn preserving_folders_keeps_empty_directories() {
        let (mut config, connection) = fixture();
        config.folders = vec![
            "生产".into(),
            "生产/Web".into(),
            "生产/空目录".into(),
            "测试".into(),
        ];
        let scope = ConnectionExportScope::Folder("生产".into());
        let folders = scope.folders(&config, false);
        let json = build_json(
            &config,
            &[connection],
            folders,
            scope.include_connection_group(),
            None,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["folders"][0], "生产");
        assert_eq!(value["folders"][1], "生产/Web");
        assert_eq!(value["folders"][2], "生产/空目录");
        assert_eq!(value["connections"][0]["group"], "生产/Web");
    }

    #[test]
    fn single_connection_export_removes_group() {
        let (config, connection) = fixture();
        let scope = ConnectionExportScope::Connection(connection.name.clone());
        let json = build_json(
            &config,
            &[connection],
            scope.folders(&config, true),
            scope.include_connection_group(),
            None,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("folders").is_none());
        assert_eq!(value["connections"][0]["group"], "");
    }

    #[test]
    fn importing_single_connection_without_group_uses_root() {
        let (source, connection) = fixture();
        let json = build_json(&source, &[connection], None, false, None).unwrap();
        let package = decode_json(&json, None).unwrap();
        let mut target = AppConfig::default();
        let summary = apply_import(&mut target, package).unwrap();
        assert_eq!(summary.added, 1);
        assert_eq!(summary.merged, 0);
        assert_eq!(target.connections[0].group, "");
        assert!(target.folders.is_empty());
    }

    #[test]
    fn importing_grouped_connections_without_folders_rebuilds_group_path() {
        let (mut source, first) = fixture();
        let second = SavedConnection {
            name: "deploy@example.net:22".into(),
            host: "example.net".into(),
            port: 22,
            user: "deploy".into(),
            group: "生产/Web".into(),
            key_path: "~/.ssh/id_ed25519".into(),
            ..Default::default()
        };
        source.connections.push(second.clone());
        let json = build_json(&source, &[first, second], None, true, None).unwrap();
        let package = decode_json(&json, None).unwrap();
        let mut target = AppConfig::default();
        let summary = apply_import(&mut target, package).unwrap();
        assert_eq!(summary.added, 2);
        assert_eq!(
            target.folders,
            vec!["生产".to_string(), "生产/Web".to_string()]
        );
        assert!(
            target
                .connections
                .iter()
                .all(|connection| connection.group == "生产/Web")
        );
    }

    #[test]
    fn importing_folders_restores_empty_directories() {
        let (source, connection) = fixture();
        let folders = Some(vec!["生产".into(), "生产/Web".into(), "生产/空目录".into()]);
        let json = build_json(&source, &[connection], folders, true, None).unwrap();
        let package = decode_json(&json, None).unwrap();
        let mut target = AppConfig::default();
        let summary = apply_import(&mut target, package).unwrap();
        assert_eq!(summary.folders_added, 3);
        assert!(target.folders.contains(&"生产/空目录".to_string()));
    }

    #[test]
    fn importing_duplicate_connection_merges_without_duplication() {
        let (source, connection) = fixture();
        let json = build_json(&source, &[connection], None, true, None).unwrap();
        let package = decode_json(&json, None).unwrap();
        let mut target = AppConfig::default();
        target.connections.push(SavedConnection {
            name: "root@example.com:22".into(),
            host: "example.com".into(),
            port: 22,
            user: "root".into(),
            alias: "旧名称".into(),
            group: "旧目录".into(),
            tags: vec!["existing".into()],
            ..Default::default()
        });
        let summary = apply_import(&mut target, package).unwrap();
        assert_eq!(summary.added, 0);
        assert_eq!(summary.merged, 1);
        assert_eq!(target.connections.len(), 1);
        assert_eq!(target.connections[0].alias, "生产服务器");
        assert_eq!(target.connections[0].group, "生产/Web");
        assert_eq!(
            target.connections[0].tags,
            vec!["existing".to_string(), "prod".to_string()]
        );
    }
}
