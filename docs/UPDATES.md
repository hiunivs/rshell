# RShell 安全更新发布

RShell 只统一更新设置和入口，不在主进程中下载、解压或覆盖可执行文件：

- macOS：`Sparkle.framework` 验证 EdDSA 与代码签名并执行原子更新。
- Windows：签名的 MSIX Bundle 由 Windows App Installer 安装、更新和修复。

项目以开源方式发布，**不需要 Apple Developer 账号**：所有构建都在 GitHub Actions 完成，安装包与更新清单一并托管在 GitHub Releases。macOS 使用固定的自签名代码签名证书（保证 Sparkle 新旧版本签名一致性校验通过），不做 Apple 公证；Windows 使用自签名证书签名 MSIX。

本地 `cargo run` 没有内嵌 Sparkle，也没有正式更新 URL，因此设置页会显示“未配置”。这是预期行为。

## GitHub Releases 托管结构

| 内容 | 位置 |
| --- | --- |
| 各版本安装包与源码归档 | `v<version>` Release（不可变资产，同名覆盖会失败） |
| macOS 更新清单 | `stable`（或 `beta`）Release 中的 `appcast.xml`，每次发布原子替换 |
| macOS 最新 ZIP | 同一 channel Release，下载前缀与 appcast 中 enclosure 地址一致 |
| Windows 更新清单 | channel Release 中的 `RShell.appinstaller`，每次发布原子替换 |
| Windows 最新 MSIX Bundle | 同一 channel Release |

默认编译期地址（CI 输入已预填）：

```text
macOS appcast:  https://github.com/hiunivs/rshell/releases/download/stable/appcast.xml
Windows 更新目录: https://github.com/hiunivs/rshell/releases/download/stable
```

用户首次打开 macOS 应用时 Gatekeeper 会提示未公证，需要右键 → 打开；Windows 首次安装前需信任自签名证书（双击证书 → 安装到“受信任的根证书颁发机构”，或按 Release 说明操作）。此后版本更新由 Sparkle / App Installer 自动完成，不再重复提示。

## 版本号

`Cargo.toml` 的 package version 是唯一的用户版本号。每次正式发布还必须提供单调递增的 `RSHELL_BUILD_NUMBER`：

- macOS：`CFBundleShortVersionString=<Cargo version>`，`CFBundleVersion=<build number>`。
- Windows：`<major>.<minor>.<patch>.<build number>`，每一段不得超过 65535。

不要重复使用或降低构建号。

## macOS

从 Sparkle 官方 Release 获取并固定一个经过审核的 Sparkle 2 版本。发布脚本不会自动下载最新版本，防止构建时供应链内容漂移。

首次使用 Sparkle 的 `generate_keys` 生成 EdDSA 密钥。私钥保留在发布机器钥匙串中，只把公钥传给构建。

代码签名使用**固定的自签名证书**（无需 Apple 账号）：打开“钥匙串访问 → 证书助理 → 创建证书”，身份类型选“自签名根证书”，证书类型选“代码签名”，名称例如 `RShell Self-Signed`。创建后不要重新生成或更换，所有版本必须用同一张证书签名，否则 Sparkle 会拒绝更新。

构建前设置：

```bash
export RSHELL_BUILD_NUMBER=100
export RSHELL_MACOS_APPCAST_URL=https://github.com/hiunivs/rshell/releases/download/stable/appcast.xml
export RSHELL_MACOS_BETA_APPCAST_URL=https://github.com/hiunivs/rshell/releases/download/beta/appcast.xml
export RSHELL_SPARKLE_PUBLIC_KEY='BASE64_PUBLIC_KEY'
export RSHELL_MACOS_SIGN_IDENTITY='RShell Self-Signed'
export SPARKLE_FRAMEWORK_PATH=/verified/Sparkle.framework
export SPARKLE_LICENSE_PATH=/verified/LICENSE
./packaging/macos/package.sh
```

脚本将：

1. 构建 Apple silicon 与 Intel 两种架构并合并为 Universal Binary。
2. 嵌入 Sparkle，生成 Info.plist。
3. 把 RShell、Rust 依赖、字体、图标和 Sparkle 的许可证材料写入 App Bundle。
4. 使用 Hardened Runtime 签名并验证整个 App Bundle。
5. 生成最终 ZIP。
6. 从当前版本 tag 导出源码并通过 `cargo vendor --locked` 一并收录依赖源码。

如以后拥有 Apple Developer 账号，可额外设置 `RSHELL_MACOS_SIGN_IDENTITY` 为 Developer ID 身份并配置 `RSHELL_APPLE_NOTARY_PROFILE`（先用 `xcrun notarytool store-credentials` 创建 profile），脚本会自动恢复公证与 stapler 流程。注意更换签名证书会让旧版本的 Sparkle 签名校验失败，需要按 Sparkle 的签名迁移流程处理。

将最终 ZIP 放入仅包含待发布更新归档的目录，使用同一 Sparkle 发行版生成并签名 appcast：

```bash
export SPARKLE_BIN_DIR=/verified/Sparkle/bin
export RSHELL_MACOS_DOWNLOAD_PREFIX=https://github.com/hiunivs/rshell/releases/download/stable
./packaging/macos/generate_appcast.sh
```

Sparkle 2.9+ 的 `generate_appcast` 会检测归档 App 中的 `SURequireSignedFeed`，并同时签署更新归档、appcast 和外链 release notes；内嵌 release notes 不需要单独文件签名。生成后不能手工修改这些文件，任何修改都必须重新运行该工具。

发布前验证：

```bash
codesign --verify --deep --strict --verbose=2 dist/macos/RShell.app
# 仅在启用了公证时：
xcrun stapler validate dist/macos/RShell.app
spctl --assess --type execute --verbose=2 dist/macos/RShell.app
```

## Windows

安装 Visual Studio Build Tools、Rust MSVC target 和 Windows 10/11 SDK。准备代码签名证书；证书的 Subject 必须与 `Publisher` 完全一致。项目使用自签名证书即可，可用 PowerShell 生成：

```powershell
$cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=RShell" `
    -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddYears(10)
$password = Read-Host -AsSecureString "PFX password"
Export-PfxCertificate -Cert $cert -FilePath rshell-signing.pfx -Password $password
```

把证书安全导入当前发布机器的证书存储并按 thumbprint 签名：

```powershell
$env:RSHELL_WINDOWS_CERT_SHA1 = "CERTIFICATE_THUMBPRINT"
./packaging/windows/package.ps1 `
    -Architecture x64 `
    -BuildNumber 100 `
    -Publisher "CN=RShell" `
    -PublisherDisplayName "RShell" `
    -UpdateBaseUrl "https://github.com/hiunivs/rshell/releases/download/stable" `
    -BetaUpdateBaseUrl "https://github.com/hiunivs/rshell/releases/download/beta"
```

CI 临时发布机也可以使用受保护的 PFX secret：

```powershell
$env:RSHELL_WINDOWS_PFX_PATH = "C:\secure\rshell-signing.pfx"
$env:RSHELL_WINDOWS_PFX_PASSWORD = "<从 CI/本机 secret manager 注入>"
./packaging/windows/package.ps1 `
    -Architecture x64 `
    -BuildNumber 100 `
    -Publisher "CN=RShell" `
    -PublisherDisplayName "RShell" `
    -UpdateBaseUrl "https://github.com/hiunivs/rshell/releases/download/stable" `
    -BetaUpdateBaseUrl "https://github.com/hiunivs/rshell/releases/download/beta"
```

输出：

```text
dist/windows/stable/RShell-<version>-windows-x64.msixbundle
dist/windows/stable/RShell.appinstaller
dist/source/RShell-<version>-source.zip
```

发布测试版时增加 `-ReleaseChannel beta`，产物会写入 `dist/windows/beta`；同一构建内的稳定版和测试版 URL 都是编译期固定值。CI 中 macOS 和 Windows 的产物通道分别选择，测试版通道必须填写对应的 beta URL。

发布脚本在生成任何 `.appinstaller` 之前强制签名并用 SignTool 验证 Bundle。不要发布脚本中间目录里的未签名 MSIX。

如果增加 arm64，在发布同一版本前分别生成 x64/arm64 MSIX，并把两者放入同一个 bundle 输入目录重新生成、签名 Bundle。当前脚本每次生成一个架构的 Bundle。

GitHub Releases 会为 `.appinstaller`、`.msixbundle` 和 ZIP 提供稳定的 HTTPS 地址与正确的 Content-Length。Windows 应用内按钮直接打开 `.appinstaller` 地址；浏览器下载后由用户打开，已通过 App Installer 安装的版本则按清单每 12 小时最多检查一次。

## 发布权限

- 正式构建要求工作区干净，且当前提交必须存在与 `Cargo.toml` 完全一致的 `v<version>` tag；例如版本 `0.1.1` 必须从 `v0.1.1` 构建。
- 签名、appcast 生成和发布只允许从受保护的 release environment 运行，不能在来自 fork 的 PR 中运行。
- 自签名证书私钥、Sparkle EdDSA 私钥、PFX 密码分别保存；任何私钥都不能写入仓库或应用包。
- CI 的 publish job 先上传 `v<version>` Release 的全部版本化资产，再原子替换 channel Release 中的 appcast/`.appinstaller`，保证清单不会先指向尚未上传的包。
- `v<version>` Release 中的资产不可变：同名文件不能覆盖，重发同一版本必须先删除旧资产并重新走完整流程。
- 对应的 `*-source.tar.gz` 或 `*-source.zip` 必须与二进制放在同一发布位置并保持可访问；只要仍提供该二进制，就不能提前删除其对应源码。
- 保留至少一个已知可用的旧安装包。回滚时发布版本号更高、内容恢复到旧实现的新包，不降低版本号。

## 必测场景

- 修改 ZIP、appcast、MSIX 任意字节后必须被拒绝。
- 使用错误签名证书、Sparkle 公钥或 Windows Publisher 必须被拒绝。
- 安装中断后旧版本仍可启动，用户配置与密钥不丢失。
- 从至少两个历史版本升级到当前版本。
- macOS Intel/Apple silicon，Windows x64/arm64，普通用户权限。

正式发布前还应先用 beta channel Release 完整演练一次更新流程，再发布到 stable。

## CI

仓库包含一个只允许手动触发、绑定 `release` protected environment 的工作流：[build-release-artifacts.yml](../.github/workflows/build-release-artifacts.yml)。它构建并上传短期保留的签名产物，随后（默认 `publish=true`）自动创建 `v<version>` Release 并上传安装包与源码，最后更新 `stable`/`beta` channel Release 中的更新清单。整个发布过程不需要 Apple 凭据，也不需要自建更新服务器。

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

### macOS 自签名证书

按上文“macOS”章节的步骤创建固定的自签名代码签名证书后，在“钥匙串访问”中将证书连同私钥导出为有密码保护的 `rshell-release.p12`，再编码为单行 Base64：

```bash
openssl base64 -A -in rshell-release.p12
```

将结果配置为以下 `release` Environment secrets：

| Secret | 内容 |
| --- | --- |
| `MACOS_CERTIFICATE_BASE64` | `.p12` 文件的单行 Base64 |
| `MACOS_CERTIFICATE_PASSWORD` | 导出 `.p12` 时设置的密码 |
| `MACOS_SIGN_IDENTITY` | 自签名证书的完整名称，例如 `RShell Self-Signed` |
| `RELEASE_KEYCHAIN_PASSWORD` | CI 临时钥匙串使用的随机密码 |

可生成一个独立的临时钥匙串密码：

```bash
openssl rand -base64 32
```

`RELEASE_KEYCHAIN_PASSWORD` 不是证书密码，不能与证书密码复用。无论构建成功或失败，工作流最后都会删除临时钥匙串。

### macOS Sparkle 更新签名密钥

Sparkle EdDSA 密钥与代码签名证书相互独立：代码签名证书对 App 签名并保持新旧版本身份一致，Sparkle EdDSA 对更新归档和更新清单签名。

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

构建时，公钥会写入 App 的 `SUPublicEDKey`；CI 解码私钥文件后通过 `generate_appcast --ed-key-file` 直接读取并签署更新 ZIP 和 `appcast.xml`，不经过钥匙串，避免无头 runner 上出现交互式授权弹窗。

必须离线备份 Sparkle 私钥。第一个正式版本发布后，不要直接重新生成并替换密钥，因为已安装的旧版本仍然内置原公钥；需要更换时必须使用 Sparkle 的密钥轮换流程。

### Windows MSIX 签名证书

项目使用自签名代码签名证书（生成命令见上文“Windows”章节），导出为有密码保护、包含私钥的 `.pfx`。每台首次安装的设备都需要先手动信任该证书；此后的更新由 App Installer 自动完成。

在 Windows PowerShell 中读取证书 Subject：

```powershell
(Get-PfxCertificate "C:\secure\rshell-signing.pfx").Subject
```

输出类似：

```text
CN=RShell
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

- macOS：`SPARKLE_PUBLIC_KEY`、`SPARKLE_PRIVATE_KEY_BASE64`、`MACOS_SIGN_IDENTITY`、`MACOS_CERTIFICATE_BASE64`、`MACOS_CERTIFICATE_PASSWORD`、`RELEASE_KEYCHAIN_PASSWORD`。
- Windows：`WINDOWS_PFX_BASE64`、`WINDOWS_PFX_PASSWORD`。

发布使用工作流内置的 `GITHUB_TOKEN`（publish job 具有 `contents: write` 权限），无需额外配置。

### 运行签名构建

进入 GitHub 仓库的 `Actions -> Build and publish release -> Run workflow`。URL 输入已预填 GitHub Releases 地址，通常只需填写：

```text
build_number:                    100
macos_release_channel:           stable
windows_release_channel:         stable
windows_publisher:               CN=RShell
windows_publisher_display_name:  RShell
publish:                         true
```

工作流会构建、签名并验证以下文件，将它们保存为保留 14 天的 GitHub Actions Artifacts，并在 `publish=true` 时自动发布：

```text
macOS:
RShell-<version>-macos-universal.zip
RShell-<version>-source.tar.gz
appcast.xml

Windows:
RShell-<version>-windows-x64.msixbundle
RShell.appinstaller
RShell-<version>-source.zip
```

工作流首先用固定版本的 `cargo-about` 重新生成 Rust 锁定依赖的许可证报告，并与仓库中的 `THIRD_PARTY_LICENSES.html` 比对；报告过期或出现未接受许可证时会中止发布。macOS 与 Windows 安装包都会携带项目许可证、第三方许可证报告以及直接嵌入资源的许可证正文。

发布由 publish job 完成：先创建 `v<version>` Release 并上传全部版本化资产（不可覆盖），再向对应 channel Release（`stable` 或 `beta`）上传最新安装包，最后以 `--clobber` 原子替换 `appcast.xml` / `RShell.appinstaller`。客户端因此永远不会拿到指向不存在安装包的清单。`publish=false` 时只产出 Actions Artifacts，可人工验证后再手动发布。

工作流中的第三方 Action 使用不可变提交 SHA，Rust 工具链由仓库根目录的 `rust-toolchain.toml` 固定。
