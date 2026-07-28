# RShell

用 Rust 实现的 FinalShell 风格 SSH 终端:左侧系统监控、中间终端、下方 SFTP 文件管理器、顶部多会话标签页。

## 功能

- **SSH 会话**:密码 / 私钥认证,多标签页,断线提示;已保存连接
- **主机指纹校验(TOFU)**:首次连接弹窗确认 SHA256 指纹,指纹变更时强警告(`known_hosts.toml`)
- **密码安全存储**:默认使用 App 内 AES-256-GCM 加密，也可切换到系统钥匙串；选中已保存连接自动回填
- **终端**:基于 alacritty_terminal 的完整终端模拟(ANSI/256 色/滚回/选区复制/括号粘贴/IME 中文输入)
- **本机终端**:在新建连接标签页中直接打开本地 shell(portable-pty,跨平台)
- **系统监控侧栏**:运行时间/负载/CPU/内存/交换、进程 Top10、网络速率双折线图、延迟、磁盘用量(远程经 SSH exec 采集 `/proc`,本机经后台 sysinfo 线程每秒采样)
- **文件管理器**:SFTP/本机统一浏览,目录树懒加载、排序、重命名/删除/新建、上传下载带进度、拖拽文件到面板直接上传
- **命令输入条**:回车发送到当前终端,↑↓ 翻阅历史,常用命令一键执行

## 构建运行

```bash
cargo run --release
cargo test          # 单元与回环测试：pty、SSH、监控解析、文件服务、known_hosts、存储工具
cargo test -- --ignored   # 另跑真实系统 keyring 读写测试
```

## 技术栈

egui 0.35 · alacritty_terminal · russh + russh-sftp · portable-pty · sysinfo · keyring · tokio

## 已知限制

- Windows/Linux 尚未实机验证(代码按跨平台编写)
- 不支持键盘交互认证(keyboard-interactive,常见于需要二次验证的服务器)
