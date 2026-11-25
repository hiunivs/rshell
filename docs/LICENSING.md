# RShell 双许可证说明

RShell 采用“AGPL-3.0-only 社区授权 + 单独书面商业授权”的双许可证模式。本文帮助使用者和发布负责人选择路径，不构成法律意见；发生冲突时，以 [`LICENSE`](../LICENSE)、[`NOTICE`](../NOTICE) 和双方实际签署的商业协议为准。

## 社区授权

选择社区授权时，须遵守 GNU Affero General Public License v3（仅 v3）以及 NOTICE 中依据第 7(b) 条加入的合理署名要求。常见场景包括：

- 分发程序或修改版时，向接收者提供许可证和对应源代码，并保留版权、无担保和许可证声明。
- 修改后的程序支持用户通过网络远程交互时，向这些用户显著提供取得该版本对应源代码的入口。
- 交互界面的“关于”“致谢”或“帮助”等易于访问的位置须保留 `Based on RShell`、版权主体和源代码地址。
- 修改版须显著说明已经修改并标注相关日期，不得冒充官方原版。

是否构成衍生作品、组合分发是否触发整体开源，以及具体源码提供方式取决于实际技术和发布方式；存在疑问时应让熟悉开源许可证的律师审查。

## 商业授权

不希望遵守 AGPL 的源码开放和传播条件时，必须在使用或发布前与 hiunivs 另行签署商业授权。仓库中的 [`COMMERCIAL-LICENSE.md`](../COMMERCIAL-LICENSE.md) 只是联系和范围说明，本身不授予商业权利。

商业授权联系：`ai-cube@outlook.com`。

## 发布检查

正式安装包必须同时携带：

1. 未修改的 AGPL-3.0-only 全文；
2. 项目 NOTICE 与商业授权说明；
3. `THIRD_PARTY_NOTICES.md` 和由锁定依赖生成的 `THIRD_PARTY_LICENSES.html`；
4. 内嵌字体、图标和 Sparkle（仅 macOS）的许可证正文；
5. 与该二进制版本完全对应的源码归档，或与下载二进制同等便利且长期有效的对应源码入口。

发布脚本会把这些材料放入 macOS App 的 `Contents/Resources/Licenses` 或 Windows MSIX 的 `Licenses` 目录，并从当前版本 tag 生成包含锁定 Cargo 依赖源码的归档。只应从与 `Cargo.toml` 版本一致的 release tag 构建正式包。

## 接受外部贡献

只有原始版权持有人及取得充分授权的人才能对同一代码提供两种授权。因此，外部贡献在合并前必须按 [`CONTRIBUTING.md`](../CONTRIBUTING.md) 完成贡献者协议；仅有 DCO 或在 PR 中同意 AGPL，通常不足以让项目继续为该贡献提供商业授权。
