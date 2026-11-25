// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! 本地 shell 会话:portable-pty 起一个伪终端,读写各用一个线程。

use std::io::{Read, Write};
use std::sync::mpsc::channel;

use anyhow::Context as _;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::files::{self, sftp_channel};
use super::{SessionChannels, SessionCmd, SessionEvent, cmd_channel, data_channel};

/// 启动本地 shell + 文件服务,返回会话通道。
pub fn spawn(cols: u16, rows: u16, ctx: egui::Context) -> anyhow::Result<SessionChannels> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty failed")?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| {
        #[cfg(windows)]
        {
            "powershell.exe".to_string()
        }
        #[cfg(not(windows))]
        {
            "/bin/sh".to_string()
        }
    });
    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    if let Ok(home) = std::env::var("HOME") {
        cmd.cwd(home);
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("spawn shell failed")?;

    // Unix 上必须尽快释放 slave 端,否则子进程退出后 master 读不到 EOF;
    // Windows ConPTY 上 slave 需要保活,放到等待线程里持有。
    #[cfg(unix)]
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone reader")?;
    let mut writer = pair.master.take_writer().context("take writer")?;
    let master = pair.master;

    let (cmd_tx, mut cmd_rx) = cmd_channel();
    let (data_tx, data_rx) = data_channel(ctx.clone());
    let (event_tx, event_rx) = channel::<SessionEvent>();

    // 文件服务线程(本机目录浏览/文件操作)
    let (sftp_tx, sftp_rx) = sftp_channel();
    files::spawn_local_worker(sftp_rx, event_tx.clone(), ctx.clone());

    // 读线程:pty 输出 → 事件通道 → UI
    std::thread::spawn(move || {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = event_tx.send(SessionEvent::Exited);
                    ctx.request_repaint();
                    break;
                }
                Ok(n) => {
                    if data_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = event_tx.send(SessionEvent::Exited);
                    ctx.request_repaint();
                    break;
                }
            }
        }
    });

    // 写线程:UI 输入/resize → pty;持有 child 与(Windows 的)slave,
    // 当 UI 侧所有 Sender 都 drop(标签页关闭)时杀掉子进程。
    std::thread::spawn(move || {
        #[cfg(not(unix))]
        let _slave = pair.slave;
        while let Some(cmd) = cmd_rx.blocking_recv() {
            match cmd {
                SessionCmd::Input(data) => {
                    if writer.write_all(&data).is_err() {
                        break;
                    }
                }
                SessionCmd::Resize { cols, rows } => {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                // 主动断开:跳出循环,统一走 child.kill + wait
                SessionCmd::Shutdown => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    });

    Ok(SessionChannels {
        cmd_tx,
        data_rx,
        event_rx,
        sftp_tx,
        monitor_active_tx: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 端到端验证 pty:写入 echo 命令,读回应含标记串
    #[test]
    fn echo_roundtrip() {
        let mut ch = spawn(80, 24, egui::Context::default()).unwrap();
        let marker = "RSHELL_TEST_MARKER_7f3a9";
        ch.cmd_tx
            .send(SessionCmd::Input(format!("echo {marker}\r").into_bytes()))
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            while let Ok(data) = ch.data_rx.try_recv() {
                output.extend_from_slice(&data);
                if String::from_utf8_lossy(&output).contains(marker) {
                    return;
                }
            }
            match ch.event_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(SessionEvent::Exited) => panic!("shell exited unexpectedly"),
                Ok(SessionEvent::Metrics(_)) => {}
                Ok(SessionEvent::Sftp(_)) => {}
                Ok(SessionEvent::VerifyHostKey(_)) => {}
                Ok(SessionEvent::KeyboardInteractive(_)) => {}
                Err(_) => {}
            }
        }
        panic!(
            "marker not found in output: {}",
            String::from_utf8_lossy(&output)
        );
    }
}
