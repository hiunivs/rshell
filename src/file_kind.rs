// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! 远程文件类型判定:不下载整个文件,逐级推断 文本/可执行/二进制。
//! 层级 1:unix 执行位(来自目录列表的 permissions,零请求);
//! 层级 2:文件名/扩展名(零请求);
//! 层级 3:远程 `file --mime-type` 命令(session 层 SftpCmd::DetectKind);
//! 层级 4:前 N 字节内容嗅探(magic number / NUL 字节 / UTF-8 校验)。
//! 本模块实现 1、2、4 的纯函数部分,以及层级 3 结果的解释。

use crate::session::files::FileEntry;

/// 判定结果。Executable 与 Binary 都不可内建编辑,
/// 区分是为了提示语更准确("可执行文件" vs "二进制文件")。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Text,
    Executable,
    Binary,
    /// 无法确定,需要内容嗅探(层级 4)或 file 命令(层级 3)进一步判定
    Unknown,
}

/// 层级 2:常见文本扩展名(小写,不含点)
const TEXT_EXTS: &[&str] = &[
    "txt",
    "md",
    "markdown",
    "log",
    "conf",
    "cfg",
    "config",
    "ini",
    "toml",
    "yaml",
    "yml",
    "json",
    "jsonl",
    "xml",
    "csv",
    "tsv",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ksh",
    "py",
    "pyi",
    "rb",
    "pl",
    "pm",
    "rs",
    "go",
    "c",
    "h",
    "cpp",
    "hpp",
    "cc",
    "cxx",
    "java",
    "kt",
    "js",
    "mjs",
    "ts",
    "jsx",
    "tsx",
    "css",
    "scss",
    "less",
    "html",
    "htm",
    "vue",
    "svelte",
    "sql",
    "lua",
    "vim",
    "el",
    "lisp",
    "clj",
    "hs",
    "ml",
    "swift",
    "php",
    "r",
    "jl",
    "ex",
    "exs",
    "erl",
    "hrl",
    "service",
    "timer",
    "socket",
    "mount",
    "env",
    "properties",
    "gradle",
    "cmake",
    "mk",
    "diff",
    "patch",
    "po",
    "pot",
    "srt",
    "ass",
    "gitignore",
    "gitattributes",
    "gitmodules",
    "dockerignore",
    "editorconfig",
    "npmignore",
    "clang-format",
    "clang-tidy",
];

/// 层级 2:常见二进制扩展名(小写,不含点)
const BINARY_EXTS: &[&str] = &[
    "zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", "zst", "lz4", "png", "jpg", "jpeg", "gif",
    "bmp", "webp", "ico", "icns", "tiff", "pdf", "so", "dll", "dylib", "a", "o", "obj", "elf",
    "bin", "dat", "exe", "msi", "dmg", "pkg", "deb", "rpm", "apk", "ipa", "jar", "war", "class",
    "pyc", "pyo", "wasm", "db", "sqlite", "sqlite3", "mdb", "mp3", "mp4", "m4a", "mov", "avi",
    "mkv", "flv", "wmv", "wav", "flac", "ogg", "opus", "ttf", "otf", "woff", "woff2", "eot", "p12",
    "pfx", "der", "crt", "cer", "img", "iso", "vmdk", "qcow2",
];

/// 层级 2:无扩展名的常见文本文件名(小写)
const TEXT_NAMES: &[&str] = &[
    "makefile",
    "gnumakefile",
    "dockerfile",
    "containerfile",
    "vagrantfile",
    "rakefile",
    "gemfile",
    "brewfile",
    "procfile",
    "license",
    "licence",
    "copying",
    "authors",
    "changelog",
    "changes",
    "contributors",
    "notice",
    "readme",
    "todo",
    "news",
    "install",
    "configure",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".bash_logout",
    ".vimrc",
    ".nanorc",
    ".gitconfig",
    ".gitignore",
    ".gitattributes",
    ".ssh/config",
    ".inputrc",
    ".editorconfig",
    ".env",
    ".aliases",
    ".exports",
    ".tmux.conf",
    ".xinitrc",
];

/// 层级 1 + 2:只看目录列表已有的元数据(不读内容)。
pub fn from_entry(entry: &FileEntry) -> FileKind {
    if entry.is_dir {
        return FileKind::Unknown;
    }
    let name = entry.name.to_lowercase();
    let ext = name.rsplit('.').next().unwrap_or("");
    let has_ext = name.contains('.') && !name.starts_with('.') || name.matches('.').count() > 1;
    // 文本扩展名优先于执行位:.sh / .py 加了 +x 仍然是可编辑文本
    if has_ext && TEXT_EXTS.contains(&ext) {
        return FileKind::Text;
    }
    if TEXT_NAMES.contains(&name.as_str()) {
        return FileKind::Text;
    }
    if has_ext && BINARY_EXTS.contains(&ext) {
        return FileKind::Binary;
    }
    // 层级 1:有执行位且不是已知文本 → 很可能是二进制可执行文件
    if entry.permissions & 0o111 != 0 {
        return FileKind::Executable;
    }
    FileKind::Unknown
}

/// 层级 4:内容嗅探。调用方一般传文件的前 4~8KB。
pub fn sniff(bytes: &[u8]) -> FileKind {
    if bytes.is_empty() {
        return FileKind::Text;
    }
    // 可执行格式魔数
    if bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(b"MZ") // PE/Windows
        || bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) // Mach-O 64 LE
        || bytes.starts_with(&[0xfe, 0xed, 0xfa, 0xcf]) // Mach-O 64 BE
        || bytes.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
    // Mach-O fat / Java class
    {
        return FileKind::Executable;
    }
    // shebang 脚本:可执行但本质是文本,允许编辑
    if bytes.starts_with(b"#!") {
        return FileKind::Text;
    }
    // 常见二进制格式魔数
    const MAGICS: &[&[u8]] = &[
        b"\x89PNG\r\n\x1a\n",
        b"\xff\xd8\xff", // JPEG
        b"GIF87a",
        b"GIF89a",
        b"BM", // BMP(误判风险低:文本很少以 BM 开头且随后两字是文件大小)
        b"%PDF-",
        b"PK\x03\x04",         // zip/jar/apk
        b"PK\x05\x06",         // 空 zip
        b"\x1f\x8b",           // gzip
        b"\xfd7zXZ\x00",       // xz
        b"7z\xbc\xaf\x27\x1c", // 7z
        b"Rar!\x1a\x07",       // rar
        b"\x28\xb5\x2f\xfd",   // zstd
        b"SQLite format 3\x00",
        b"\x00asm", // wasm
        b"OggS",
        b"fLaC",
        b"ID3", // mp3
    ];
    if MAGICS.iter().any(|m| bytes.starts_with(m)) {
        return FileKind::Binary;
    }
    // NUL 字节几乎不会出现在文本里
    if bytes.contains(&0) {
        return FileKind::Binary;
    }
    if std::str::from_utf8(bytes).is_ok() {
        return FileKind::Text;
    }
    // 非 UTF-8 又无 NUL:可能是 latin-1/GBK 文本,留给层级 3 判定
    FileKind::Unknown
}

/// 层级 3:解释 `file --mime-type -b` 的输出。
pub fn from_mime(mime: &str) -> FileKind {
    let m = mime.trim().to_lowercase();
    if m.starts_with("text/") {
        return FileKind::Text;
    }
    match m.as_str() {
        "application/json"
        | "application/xml"
        | "application/javascript"
        | "application/x-javascript"
        | "application/x-sh"
        | "application/x-shellscript"
        | "application/x-perl"
        | "application/x-python"
        | "application/x-ruby"
        | "application/x-yaml"
        | "application/x-toml"
        | "application/sql"
        | "inode/x-empty" => FileKind::Text,
        "application/x-executable"
        | "application/x-pie-executable"
        | "application/x-sharedlib"
        | "application/x-mach-binary"
        | "application/x-elf"
        | "application/x-dosexec" => FileKind::Executable,
        _ => FileKind::Binary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, permissions: u32) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir: false,
            size: 0,
            mtime: 0,
            permissions,
            uid: 0,
            gid: 0,
            owner: String::new(),
            group: String::new(),
            is_symlink: false,
            is_broken_symlink: false,
        }
    }

    #[test]
    fn text_ext_beats_exec_bit() {
        // chmod +x 的 shell 脚本仍然是文本
        assert_eq!(from_entry(&entry("deploy.sh", 0o755)), FileKind::Text);
        assert_eq!(from_entry(&entry("notes.txt", 0o644)), FileKind::Text);
        assert_eq!(from_entry(&entry("config.yaml", 0o644)), FileKind::Text);
    }

    #[test]
    fn exec_bit_flags_extensionless() {
        assert_eq!(from_entry(&entry("a.out", 0o755)), FileKind::Executable);
        assert_eq!(from_entry(&entry("server", 0o755)), FileKind::Executable);
        assert_eq!(from_entry(&entry("server", 0o644)), FileKind::Unknown);
    }

    #[test]
    fn binary_ext_detected() {
        assert_eq!(from_entry(&entry("photo.png", 0o644)), FileKind::Binary);
        assert_eq!(from_entry(&entry("app.tar.gz", 0o644)), FileKind::Binary);
        assert_eq!(from_entry(&entry("libssl.so", 0o755)), FileKind::Binary);
    }

    #[test]
    fn extensionless_text_names() {
        assert_eq!(from_entry(&entry("Makefile", 0o644)), FileKind::Text);
        assert_eq!(from_entry(&entry(".bashrc", 0o644)), FileKind::Text);
        assert_eq!(from_entry(&entry("LICENSE", 0o644)), FileKind::Text);
        assert_eq!(from_entry(&entry("Dockerfile", 0o755)), FileKind::Text);
    }

    #[test]
    fn sniff_magic_and_content() {
        assert_eq!(sniff(b""), FileKind::Text);
        assert_eq!(sniff(b"\x7fELF\x02\x01\x01\0"), FileKind::Executable);
        assert_eq!(sniff(b"#!/bin/bash\necho hi\n"), FileKind::Text);
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nabc"), FileKind::Binary);
        assert_eq!(sniff(b"PK\x03\x04zip"), FileKind::Binary);
        assert_eq!(sniff("你好,世界\n第二行\n".as_bytes()), FileKind::Text);
        assert_eq!(sniff(b"hello\x00world"), FileKind::Binary);
        // latin-1 文本:非 UTF-8、无 NUL → 交给层级 3
        assert_eq!(sniff(b"caf\xe9 au lait"), FileKind::Unknown);
    }

    #[test]
    fn mime_mapping() {
        assert_eq!(from_mime("text/plain"), FileKind::Text);
        assert_eq!(from_mime("application/json"), FileKind::Text);
        assert_eq!(from_mime("inode/x-empty"), FileKind::Text);
        assert_eq!(from_mime("application/x-executable"), FileKind::Executable);
        assert_eq!(from_mime("application/x-mach-binary"), FileKind::Executable);
        assert_eq!(from_mime("image/png"), FileKind::Binary);
        assert_eq!(from_mime("application/octet-stream"), FileKind::Binary);
    }
}
