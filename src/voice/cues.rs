//! 提示音:短促的事件音效,不用看屏幕也知道她听到了、办完了。
//!
//! 全部内嵌(24kHz 单声道 16-bit,每个 ≤30KB,木琴音色),由
//! `testkit/voice/sounds/synth.py` 生成。播放走 rodio(闹钟 worker 同款),
//! 输出流按需打开、闲置即关,不占音频设备。

use anyhow::{Context, Result};
use std::io::Cursor;
use std::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// 唤醒命中:上行两音,"请讲"。
    Wake,
    /// 指令已识别:单个短点。
    Heard,
    /// 回合完成:下行三音。
    Done,
    /// 出错:低音小二度。
    Error,
    /// 不听了(快捷键再按一次关闭):wake 的镜像,下行两音。
    Off,
}

impl Cue {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "wake" => Some(Self::Wake),
            "heard" => Some(Self::Heard),
            "done" => Some(Self::Done),
            "error" => Some(Self::Error),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

fn bytes(cue: Cue) -> &'static [u8] {
    match cue {
        Cue::Wake => include_bytes!("../../assets/voice/wake.wav"),
        Cue::Heard => include_bytes!("../../assets/voice/heard.wav"),
        Cue::Done => include_bytes!("../../assets/voice/done.wav"),
        Cue::Error => include_bytes!("../../assets/voice/error.wav"),
        Cue::Off => include_bytes!("../../assets/voice/off.wav"),
    }
}

/// 播放器句柄:自带线程,rodio 的输出流不是 Send,只能圈在线程里。
pub struct Player {
    tx: mpsc::Sender<(&'static [u8], f32)>,
}

impl Player {
    pub fn start() -> Result<Self> {
        let (tx, rx) = mpsc::channel::<(&'static [u8], f32)>();
        std::thread::Builder::new()
            .name("miyu-voice-cues".into())
            .spawn(move || {
                // 输出流按需打开:收到第一个音效才开,连续 30s 没动静就关,
                // 这样不说话时不挂着音频设备。
                let mut output: Option<(rodio::OutputStream, rodio::OutputStreamHandle)> = None;
                loop {
                    let message = if output.is_some() {
                        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                            Ok(message) => message,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                output = None;
                                continue;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    } else {
                        match rx.recv() {
                            Ok(message) => message,
                            Err(_) => return,
                        }
                    };
                    let (data, volume) = message;
                    if output.is_none() {
                        match rodio::OutputStream::try_default() {
                            Ok(pair) => output = Some(pair),
                            Err(error) => {
                                tracing::warn!("打开音频输出失败,提示音跳过: {error}");
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
                    let Ok(source) = rodio::Decoder::new_wav(Cursor::new(data)) else {
                        continue;
                    };
                    sink.set_volume(volume.clamp(0.0, 1.0));
                    sink.append(source);
                    sink.sleep_until_end();
                }
            })
            .context("启动提示音线程失败")?;
        Ok(Self { tx })
    }

    pub fn play(&self, cue: Cue, volume: f32) {
        let _ = self.tx.send((bytes(cue), volume));
    }
}
