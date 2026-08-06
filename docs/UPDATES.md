# RShell 安全更新发布

RShell 只统一更新设置和入口，不在主进程中下载、解压或覆盖可执行文件：

- macOS：`Sparkle.framework` 验证 EdDSA 与 Apple 代码签名并执行原子更新。
- Windows：签名的 MSIX Bundle 由 Windows App Installer 安装、更新和修复。

本地 `cargo run` 没有内嵌 Sparkle，也没有正式更新 URL，因此设置页会显示“未配置”。这是预期行为。

## 版本号

`Cargo.toml` 的 package version 是唯一的用户版本号。每次正式发布还必须提供单调递增的 `RSHELL_BUILD_NUMBER`：

- macOS：`CFBundleShortVersionString=<Cargo version>`，`CFBundleVersion=<build number>`。
- Windows：`<major>.<minor>.<patch>.<build number>`，每一段不得超过 65535。

不要重复使用或降低构建号。

## macOS

从 Sparkle 官方 Release 获取并固定一个经过审核的 Sparkle 2 版本。发布脚本不会自动下载最新版本，防止构建时供应链内容漂移。

首次使用 Sparkle 的 `generate_keys` 生成 EdDSA 密钥。私钥保留在发布机器钥匙串中，只把公钥传给构建。另行在钥匙串中创建 `notarytool` profile：

```bash
xcrun notarytool store-credentials RShellNotary
```

构建前设置：

```bash
export RSHELL_BUILD_NUMBER=100
export RSHELL_MACOS_APPCAST_URL=https://updates.example.com/stable/macos/appcast.xml
export RSHELL_MACOS_BETA_APPCAST_URL=https://updates.example.com/beta/macos/appcast.xml
export RSHELL_SPARKLE_PUBLIC_KEY='BASE64_PUBLIC_KEY'
export RSHELL_MACOS_SIGN_IDENTITY='Developer ID Application: Example (TEAMID)'
export RSHELL_APPLE_NOTARY_PROFILE=RShellNotary
export SPARKLE_FRAMEWORK_PATH=/verified/Sparkle.framework
./packaging/macos/package.sh
```

脚本将：

1. 构建 Apple silicon 与 Intel 两种架构并合并为 Universal Binary。
2. 嵌入 Sparkle，生成 Info.plist。
3. 使用 Hardened Runtime 签名并验证整个 App Bundle。
4. 提交 Apple Notarization、staple ticket，再生成最终 ZIP。

将最终 ZIP 放入仅包含待发布更新归档的目录，使用同一 Sparkle 发行版生成并签名 appcast：

```bash
export SPARKLE_BIN_DIR=/verified/Sparkle/bin
export RSHELL_MACOS_DOWNLOAD_PREFIX=https://updates.example.com/stable/macos
./packaging/macos/generate_appcast.sh
```

Sparkle 2.9+ 的 `generate_appcast` 会检测归档 App 中的 `SURequireSignedFeed`，并同时签署更新归档、appcast 和外链 release notes；内嵌 release notes 不需要单独文件签名。生成后不能手工修改这些文件，任何修改都必须重新运行该工具。

发布前验证：

```bash
codesign --verify --deep --strict --verbose=2 dist/macos/RShell.app
spctl --assess --type execute --verbose=2 dist/macos/RShell.app
xcrun stapler validate dist/macos/RShell.app
```

## Windows

安装 Visual Studio Build Tools、Rust MSVC target 和 Windows 10/11 SDK。准备代码签名证书；证书的 Subject 必须与 `Publisher` 完全一致。

推荐先把证书安全导入当前发布机器的证书存储并按 thumbprint 签名：

```powershell
$env:RSHELL_WINDOWS_CERT_SHA1 = "CERTIFICATE_THUMBPRINT"
./packaging/windows/package.ps1 `
    -Architecture x64 `
    -BuildNumber 100 `
    -Publisher "CN=Example" `
    -PublisherDisplayName "Example" `
    -UpdateBaseUrl "https://updates.example.com/stable/windows" `
    -BetaUpdateBaseUrl "https://updates.example.com/beta/windows"
```

CI 临时发布机也可以使用受保护的 PFX secret：

```powershell
$env:RSHELL_WINDOWS_PFX_PATH = "C:\secure\rshell-signing.pfx"
$env:RSHELL_WINDOWS_PFX_PASSWORD = "<从 CI/本机 secret manager 注入>"
./packaging/windows/package.ps1 `
    -Architecture x64 `
    -BuildNumber 100 `
    -Publisher "CN=Example" `
    -PublisherDisplayName "Example" `
    -UpdateBaseUrl "https://updates.example.com/stable/windows" `
    -BetaUpdateBaseUrl "https://updates.example.com/beta/windows"
```

输出：

```text
dist/windows/stable/RShell-<version>.msixbundle
dist/windows/stable/RShell.appinstaller
```

发布测试版时增加 `-ReleaseChannel beta`，产物会写入 `dist/windows/beta`；同一构建内的稳定版和测试版 URL 都是编译期固定值。CI 中 macOS 和 Windows 的产物通道分别选择，测试版通道必须填写对应的 beta URL。

发布脚本在生成任何 `.appinstaller` 之前强制签名并用 SignTool 验证 Bundle。不要发布脚本中间目录里的未签名 MSIX。

如果增加 arm64，在发布同一版本前分别生成 x64/arm64 MSIX，并把两者放入同一个 bundle 输入目录重新生成、签名 Bundle。当前脚本每次生成一个架构的 Bundle。

Web 服务器必须给 `.appinstaller`、`.msixbundle` 和 ZIP 返回正确的 Content-Type、Content-Length，并保持稳定 HTTPS 地址。Windows 应用内按钮直接打开 `.appinstaller` 地址；浏览器下载后由用户打开，已通过 App Installer 安装的版本则按清单每 12 小时最多检查一次。

## 发布权限

- 签名、notarization、appcast 生成只允许从受保护的 release tag/environment 运行，不能在来自 fork 的 PR 中运行。
- Developer ID 私钥、Sparkle EdDSA 私钥、PFX 密码分别保存；任何私钥都不能写入仓库或应用包。
- 上传版本文件后最后替换稳定的 appcast/`.appinstaller`，避免清单先指向尚未上传的包。
- 保留至少一个已知可用的旧安装包。回滚时发布版本号更高、内容恢复到旧实现的新包，不降低版本号。

## 必测场景

- 修改 ZIP、appcast、MSIX 任意字节后必须被拒绝。
- 使用错误 Developer ID、Sparkle 公钥或 Windows Publisher 必须被拒绝。
- 安装中断后旧版本仍可启动，用户配置与密钥不丢失。
- 从至少两个历史版本升级到当前版本。
- macOS Intel/Apple silicon，Windows x64/arm64，普通用户权限。

正式发布前还应在隔离的 staging 更新域名完整演练一次，再切换生产清单。

## CI

仓库包含一个只允许手动触发、绑定 `release` protected environment 的工作流：[build-release-artifacts.yml](../.github/workflows/build-release-artifacts.yml)。它只构建并上传短期保留的签名产物，不会自动发布到更新服务器；人工验证产物后再上传，可以避免一次构建错误直接影响所有客户端。

### 创建受保护的发布环境

进入 GitHub 仓库的 `Settings -> Environments`，创建名为 `release` 的 Environment，并配置：

- 开启 Required reviewers，正式构建前必须由发布负责人审批。
- 限制允许使用该环境的发布分支或 tag。
- 将下面列出的密码、证书和私钥保存为 Environment secrets，不要写入仓库、工作流输入或构建日志。

工作流还需要在 `Settings -> Secrets and variables -> Actions -> Variables` 中配置两个非敏感 Repository variables：

| Variable | 内容 |
| --- | --- |
| `SPARKLE_VERSION` | 固定并经过审核的 Sparkle 2.9+ 版本，例如 `2.9.2` |
| `SPARKLE_SHA256` | 对应官方 `Sparkle-<version>.tar.xz` 发行归档的 SHA-256 |

下载官方归档后可计算散列值：

```bash
shasum -a 256 Sparkle-2.9.2.tar.xz
```

工作流会再次计算并比对该值，不会使用未经验证的 Sparkle 归档。

### macOS Apple 签名证书

在 Apple Developer 账号中创建 `Developer ID Application` 证书。当前发布物是包含 `.app` 的 ZIP，不需要 `Developer ID Installer` 证书。

把证书安装到发布 Mac 的钥匙串后，查看它的完整签名名称：

```bash
security find-identity -v -p codesigning
```

名称类似：

```text
Developer ID Application: Example Technology (ABCDE12345)
```

在“钥匙串访问”中将证书连同私钥导出为有密码保护的 `rshell-release.p12`，再编码为单行 Base64：

```bash
openssl base64 -A -in rshell-release.p12
```

将结果配置为以下 `release` Environment secrets：

| Secret | 内容 |
| --- | --- |
| `MACOS_CERTIFICATE_BASE64` | `.p12` 文件的单行 Base64 |
| `MACOS_CERTIFICATE_PASSWORD` | 导出 `.p12` 时设置的密码 |
| `MACOS_SIGN_IDENTITY` | `security find-identity` 显示的完整 `Developer ID Application: ...` 名称 |
| `RELEASE_KEYCHAIN_PASSWORD` | CI 临时钥匙串使用的随机密码 |

可生成一个独立的临时钥匙串密码：

```bash
openssl rand -base64 32
```

`RELEASE_KEYCHAIN_PASSWORD` 不是 Apple 账号密码或证书密码，不能与它们复用。

### macOS Apple 公证凭据

当前工作流使用 Apple ID 和 App 专用密码调用 `notarytool`。在 Apple 账号中创建 App 专用密码，并配置：

| Secret | 内容 |
| --- | --- |
| `APPLE_ID` | Apple Developer 账号邮箱 |
| `APPLE_TEAM_ID` | 10 位 Developer Team ID |
| `APPLE_APP_PASSWORD` | Apple 账号生成的 App 专用密码，不是普通登录密码 |

CI 会将这些凭据临时保存到本次构建的钥匙串 profile 中。无论构建成功或失败，工作流最后都会删除临时钥匙串。

### macOS Sparkle 更新签名密钥

Sparkle EdDSA 密钥与 Apple Developer ID 证书相互独立：Developer ID 对 App 代码签名，Sparkle EdDSA 对更新归档和更新清单签名。

从与 `SPARKLE_VERSION` 相同的 Sparkle 官方发行包中运行一次：

```bash
./bin/generate_keys
```

命令会把私钥保存到当前 Mac 的登录钥匙串，并在终端输出 Base64 公钥。将输出的公钥保存为 `SPARKLE_PUBLIC_KEY`。然后导出供 CI 使用的私钥文件：

```bash
./bin/generate_keys -x rshell-sparkle-private-key
openssl base64 -A -in rshell-sparkle-private-key
```

配置以下 `release` Environment secrets：

| Secret | 内容 |
| --- | --- |
| `SPARKLE_PUBLIC_KEY` | `generate_keys` 输出的 Base64 公钥 |
| `SPARKLE_PRIVATE_KEY_BASE64` | 导出的私钥文件的单行 Base64 |

构建时，公钥会写入 App 的 `SUPublicEDKey`；CI 会解码私钥并用 `generate_keys -f` 导入临时钥匙串，再由 `generate_appcast` 签署更新 ZIP 和 `appcast.xml`。

必须离线备份 Sparkle 私钥。第一个正式版本发布后，不要直接重新生成并替换密钥，因为已安装的旧版本仍然内置原公钥；需要更换时必须使用 Sparkle 的密钥轮换流程。

### Windows MSIX 签名证书

正式对外发布应使用受目标 Windows 系统信任的代码签名证书，并导出为有密码保护、包含私钥的 `.pfx`。自签名证书只适合内部测试，因为每台测试设备都必须先手动信任该证书。

在 Windows PowerShell 中读取证书 Subject：

```powershell
(Get-PfxCertificate "C:\secure\rshell-signing.pfx").Subject
```

输出类似：

```text
CN=Example Technology Co., Ltd.
```

该值必须与运行工作流时填写的 `windows_publisher` 完全一致，包括大小写、空格和标点。`windows_publisher_display_name` 只是 App Installer 展示给用户的友好名称，不参与证书匹配。

将 PFX 编码成 Base64：

```powershell
$Bytes = [IO.File]::ReadAllBytes("C:\secure\rshell-signing.pfx")
[Convert]::ToBase64String($Bytes) | Set-Clipboard
```

配置以下 `release` Environment secrets：

| Secret | 内容 |
| --- | --- |
| `WINDOWS_PFX_BASE64` | PFX 文件的 Base64 |
| `WINDOWS_PFX_PASSWORD` | PFX 密码 |

本地 Windows 发布机器也可以把证书导入证书存储，并通过证书 thumbprint 签名：

```powershell
$env:RSHELL_WINDOWS_CERT_SHA1 = "CERTIFICATE_THUMBPRINT"
```

GitHub Actions 当前使用 PFX 方式；不要把 PFX 文件或密码提交到仓库。

### Secrets 和 Variables 总表

Repository variables：

- `SPARKLE_VERSION`
- `SPARKLE_SHA256`

`release` Environment secrets：

- macOS：`SPARKLE_PUBLIC_KEY`、`SPARKLE_PRIVATE_KEY_BASE64`、`MACOS_SIGN_IDENTITY`、`MACOS_CERTIFICATE_BASE64`、`MACOS_CERTIFICATE_PASSWORD`、`RELEASE_KEYCHAIN_PASSWORD`、`APPLE_ID`、`APPLE_TEAM_ID`、`APPLE_APP_PASSWORD`。
- Windows：`WINDOWS_PFX_BASE64`、`WINDOWS_PFX_PASSWORD`。

### 运行签名构建

进入 GitHub 仓库的 `Actions -> Build signed release artifacts -> Run workflow`，填写：

```text
build_number:                    100
macos_appcast_url:               https://updates.example.com/stable/macos/appcast.xml
macos_beta_appcast_url:          https://updates.example.com/beta/macos/appcast.xml
macos_release_channel:           stable
windows_update_base_url:         https://updates.example.com/stable/windows
windows_beta_update_base_url:    https://updates.example.com/beta/windows
windows_release_channel:         stable
windows_publisher:               CN=Example Technology Co., Ltd.
windows_publisher_display_name:  Example Technology
```

工作流会构建、签名并验证以下文件，然后将它们保存为保留 14 天的 GitHub Actions Artifacts：

```text
macOS:
RShell-<version>.zip
appcast.xml

Windows:
RShell-<version>.msixbundle
RShell.appinstaller
```

工作流不会自动上传到生产更新服务器。人工验证后，必须先上传带版本号的 ZIP 或 MSIX Bundle，确认下载地址可用，最后再原子替换稳定地址下的 `appcast.xml` 或 `RShell.appinstaller`，避免客户端拿到指向不存在安装包的清单。

工作流中的第三方 Action 使用不可变提交 SHA，Rust 工具链由仓库根目录的 `rust-toolchain.toml` 固定。
