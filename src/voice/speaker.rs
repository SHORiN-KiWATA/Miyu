//! 播报:在自己的线程里播 daemon 合成好的 wav。
//!
//! 播放期间通过 `on_state(true/false)` 通知宿主(worker 据此让管线丢掉麦克风
//! 帧,并告诉 daemon 正在播报);`stop()` 立即打断。

use anyhow::{Context, Result};
use std::io::Cursor;
use std::sync::mpsc;
use std::time::Duration;

pub enum SpeakerCommand {
    /// 播一段完整 wav。
    PlayWav(Vec<u8>),
    /// 打断当前播放并清空队列。
    Stop,
}

pub struct Speaker {
    tx: mpsc::Sender<SpeakerCommand>,
}

impl Speaker {
    pub fn start(on_state: Box<dyn Fn(bool) + Send>) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<SpeakerCommand>();
        std::thread::Builder::new()
            .name("miyu-voice-speaker".into())
            .spawn(move || run(rx, on_state))
            .context("启动播报线程失败")?;
        Ok(Self { tx })
    }

    pub fn play_wav(&self, wav: Vec<u8>) {
        let _ = self.tx.send(SpeakerCommand::PlayWav(wav));
    }

    pub fn stop(&self) {
        let _ = self.tx.send(SpeakerCommand::Stop);
    }
}

/// 输出流按需打开,连续 30s 没播放才关(每次重新打开设备要几十到上百毫秒,
/// 会让播报比通知晚一拍;关掉是为了不说话时不挂着音频设备)。
const OUTPUT_IDLE: Duration = Duration::from_secs(30);

fn run(rx: mpsc::Receiver<SpeakerCommand>, on_state: Box<dyn Fn(bool) + Send>) {
    let mut output: Option<(rodio::OutputStream, rodio::OutputStreamHandle)> = None;
    loop {
        let command = if output.is_some() {
            match rx.recv_timeout(OUTPUT_IDLE) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    output = None;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(command) => command,
                Err(_) => return,
            }
        };
        let SpeakerCommand::PlayWav(wav) = command else {
            continue;
        };
        if output.is_none() {
            match rodio::OutputStream::try_default() {
                Ok(pair) => output = Some(pair),
                Err(error) => {
                    tracing::warn!("打开音频输出失败,播报跳过: {error}");
                    continue;
                }
            }
        }
        let Some((_, handle)) = output.as_ref() else {
            continue;
        };
        let Ok(sink) = rodio::Sink::try_new(handle) else {
            continue;
        };
        let Ok(source) = rodio::Decoder::new(Cursor::new(wav)) else {
            tracing::warn!("播报音频解码失败");
            continue;
        };
        on_state(true);
        sink.append(source);
        wait_for_sink(&rx, &sink);
        on_state(false);
    }
}

/// 等 sink 播完;期间收到 Stop 立即停。其他命令先丢弃(播报不排队)。
fn wait_for_sink(rx: &mpsc::Receiver<SpeakerCommand>, sink: &rodio::Sink) {
    while !sink.empty() {
        let mut stop = false;
        while let Ok(command) = rx.try_recv() {
            if matches!(command, SpeakerCommand::Stop) {
                stop = true;
            }
        }
        if stop {
            sink.stop();
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    sink.sleep_until_end();
}
