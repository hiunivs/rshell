pub mod files;
pub mod local;
pub mod monitor;
pub mod ssh;

/// UI → 会话的命令
pub enum SessionCmd {
    /// 写入终端的字节(键盘输入、应答序列等)
    Input(Vec<u8>),
    /// 终端窗口尺寸变化
    Resize { cols: u16, rows: u16 },
    /// 主动断开:关闭连接/杀掉子进程后退出会话线程
    /// (UI 保留终端网格,重连时换新通道即可,滚回内容不丢)
    Shutdown,
}

/// 会话 → UI 的事件
pub enum SessionEvent {
    /// 会话进程退出或连接断开
    Exited,
    /// 一轮系统监控指标
    Metrics(std::sync::Arc<monitor::SystemMetrics>),
    /// 文件服务(SFTP/本机文件)事件
    Sftp(files::SftpEvent),
    /// 主机指纹需要用户确认(首次连接或指纹变更)
    VerifyHostKey(Box<HostKeyPrompt>),
}

/// 主机指纹确认请求:session 线程发出后阻塞等待 UI 通过 reply 回复
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    pub status: crate::known_hosts::HostKeyStatus,
    pub reply: tokio::sync::mpsc::UnboundedSender<HostKeyVerdict>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostKeyVerdict {
    Reject,
    AcceptOnce,
    AcceptAndSave,
}

/// 命令通道:tokio unbounded —— UI 线程同步 send,session 侧可 async recv
pub type CmdTx = tokio::sync::mpsc::UnboundedSender<SessionCmd>;
pub type CmdRx = tokio::sync::mpsc::UnboundedReceiver<SessionCmd>;

pub fn cmd_channel() -> (CmdTx, CmdRx) {
    tokio::sync::mpsc::unbounded_channel()
}

/// 终端输出使用有界通道；每块不超过 64 KiB 时，最多积压约 16 MiB。
pub type DataTx = tokio::sync::mpsc::Sender<Vec<u8>>;
pub type DataRx = tokio::sync::mpsc::Receiver<Vec<u8>>;

pub fn data_channel() -> (DataTx, DataRx) {
    tokio::sync::mpsc::channel(256)
}

/// 一个会话对外暴露的全部通道
pub struct SessionChannels {
    pub cmd_tx: CmdTx,
    pub data_rx: DataRx,
    pub event_rx: std::sync::mpsc::Receiver<SessionEvent>,
    pub sftp_tx: files::SftpTx,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_data_channel_has_hard_capacity() {
        let (tx, _rx) = data_channel();
        for _ in 0..256 {
            tx.try_send(vec![0; 64 * 1024]).unwrap();
        }
        assert!(matches!(
            tx.try_send(vec![0]),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));
    }
}
