// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

pub mod files;
pub mod local;
pub mod monitor;
pub mod ssh;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// SSH Keyboard-Interactive/PAM/2FA 认证问题。
    KeyboardInteractive(Box<KeyboardInteractivePrompt>),
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

pub struct KeyboardInteractiveQuestion {
    pub prompt: String,
    pub echo: bool,
}

/// Keyboard-Interactive 请求同时携带 UI 的短期响应缓冲。
/// 弹窗关闭或提交后 `Zeroizing` 会清空这些明文。
pub struct KeyboardInteractivePrompt {
    pub host: String,
    pub user: String,
    pub name: String,
    pub instructions: String,
    pub questions: Vec<KeyboardInteractiveQuestion>,
    pub responses: Vec<zeroize::Zeroizing<String>>,
    pub focus_first: bool,
    pub reply: tokio::sync::oneshot::Sender<KeyboardInteractiveVerdict>,
}

pub enum KeyboardInteractiveVerdict {
    Cancel,
    Responses(Vec<crate::secrets::SecretString>),
}

/// 命令通道:tokio unbounded —— UI 线程同步 send,session 侧可 async recv
pub type CmdTx = tokio::sync::mpsc::UnboundedSender<SessionCmd>;
pub type CmdRx = tokio::sync::mpsc::UnboundedReceiver<SessionCmd>;

pub fn cmd_channel() -> (CmdTx, CmdRx) {
    tokio::sync::mpsc::unbounded_channel()
}

/// 终端输出使用有界通道；每块不超过 64 KiB 时，最多积压约 16 MiB。
///
/// `repaint_pending` 把高频终端输出的 UI 唤醒合并为“队列从空变为非空”一次。
/// UI 排空队列后才允许生产者再次唤醒，避免每个 SSH 数据包都向 macOS 事件
/// 循环投递一次重绘请求。
#[derive(Clone)]
pub struct DataTx {
    inner: tokio::sync::mpsc::Sender<bytes::Bytes>,
    repaint_pending: Arc<AtomicBool>,
    ctx: egui::Context,
}

pub struct DataRx {
    inner: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    repaint_pending: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl DataTx {
    pub async fn send(
        &self,
        value: impl Into<bytes::Bytes>,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<bytes::Bytes>> {
        let value = value.into();
        self.inner.send(value).await?;
        self.wake_ui_once();
        Ok(())
    }

    pub fn blocking_send(
        &self,
        value: impl Into<bytes::Bytes>,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<bytes::Bytes>> {
        let value = value.into();
        self.inner.blocking_send(value)?;
        self.wake_ui_once();
        Ok(())
    }

    fn wake_ui_once(&self) {
        if !self.repaint_pending.swap(true, Ordering::AcqRel) {
            self.ctx.request_repaint();
        }
    }

    #[cfg(test)]
    fn try_send(
        &self,
        value: impl Into<bytes::Bytes>,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<bytes::Bytes>> {
        let value = value.into();
        self.inner.try_send(value)?;
        self.wake_ui_once();
        Ok(())
    }
}

impl DataRx {
    pub fn try_recv(&mut self) -> Result<bytes::Bytes, tokio::sync::mpsc::error::TryRecvError> {
        self.inner.try_recv()
    }

    /// 一帧解析结束后更新唤醒闸门。队列仍有积压时安排下一帧；已经排空时
    /// 先释放闸门再复查，避免生产者恰好在空队列检查期间发送数据而丢失唤醒。
    pub fn finish_poll(&self) {
        if !self.inner.is_empty() {
            self.ctx.request_repaint();
            return;
        }
        self.repaint_pending.store(false, Ordering::Release);
        if !self.inner.is_empty() && !self.repaint_pending.swap(true, Ordering::AcqRel) {
            self.ctx.request_repaint();
        }
    }
}

pub fn data_channel(ctx: egui::Context) -> (DataTx, DataRx) {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let repaint_pending = Arc::new(AtomicBool::new(false));
    (
        DataTx {
            inner: tx,
            repaint_pending: Arc::clone(&repaint_pending),
            ctx: ctx.clone(),
        },
        DataRx {
            inner: rx,
            repaint_pending,
            ctx,
        },
    )
}

/// 一个会话对外暴露的全部通道
pub struct SessionChannels {
    pub cmd_tx: CmdTx,
    pub data_rx: DataRx,
    pub event_rx: std::sync::mpsc::Receiver<SessionEvent>,
    pub sftp_tx: files::SftpTx,
    /// SSH 监控前后台状态；本机会话由应用级采样器统一管理。
    pub monitor_active_tx: Option<tokio::sync::watch::Sender<bool>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_data_channel_has_hard_capacity() {
        let (tx, mut rx) = data_channel(egui::Context::default());
        for _ in 0..256 {
            tx.try_send(vec![0; 64 * 1024]).unwrap();
        }
        assert!(tx.repaint_pending.load(Ordering::Acquire));
        assert!(matches!(
            tx.try_send(vec![0]),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));
        for _ in 0..256 {
            rx.try_recv().unwrap();
        }
        rx.finish_poll();
        assert!(!tx.repaint_pending.load(Ordering::Acquire));
    }
}
