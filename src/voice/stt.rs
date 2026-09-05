//! STT 引擎抽象。本地 SenseVoice 与云端 OpenAI 兼容端点各一个实现,
//! 管线侧只认 trait。

use super::models;
use anyhow::{Context, Result};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig};
use std::path::Path;

pub trait SttEngine: Send {
    /// 整段音频 → 文本。samples 为单声道 f32,幅值 [-1,1]。
    fn transcribe(&mut self, sample_rate: u32, samples: &[f32]) -> Result<String>;
    /// 模型是否已在内存中(云端实现恒 true:没有可卸载的东西)。
    fn is_loaded(&self) -> bool {
        true
    }
    /// 提前加载(唤醒/开听写时调用,把加载耗时藏进用户说话的时间里)。
    fn preload(&mut self) -> Result<()> {
        Ok(())
    }
    /// 释放模型内存(闲置卸载)。
    fn release(&mut self) {}
}

/// 本地 SenseVoice(sherpa-onnx OfflineRecognizer)。
pub struct LocalSenseVoice {
    model: std::path::PathBuf,
    tokens: std::path::PathBuf,
    threads: usize,
    /// auto | zh | en | ja | ko | yue(SenseVoice 的语言标签)。
    language: String,
    recognizer: Option<OfflineRecognizer>,
}

impl LocalSenseVoice {
    pub fn new(models_dir: &Path, threads: usize, language: &str) -> Result<Self> {
        let paths = models::sense_voice_paths(models_dir);
        anyhow::ensure!(
            paths.model.is_file(),
            "SenseVoice 模型缺失: {}",
            paths.model.display()
        );
        let language = match language.trim().to_ascii_lowercase().as_str() {
            "zh" | "en" | "ja" | "ko" | "yue" => language.trim().to_ascii_lowercase(),
            _ => "auto".to_string(),
        };
        Ok(Self {
            model: paths.model,
            tokens: paths.tokens,
            threads: threads.clamp(1, 8),
            language,
            recognizer: None,
        })
    }

    fn recognizer(&mut self) -> Result<&OfflineRecognizer> {
        if self.recognizer.is_none() {
            let mut config = OfflineRecognizerConfig::default();
            config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
                model: Some(self.model.to_string_lossy().into_owned()),
                language: Some(self.language.clone()),
                use_itn: true,
            };
            config.model_config.tokens = Some(self.tokens.to_string_lossy().into_owned());
            config.model_config.num_threads = self.threads as i32;
            self.recognizer = Some(
                OfflineRecognizer::create(&config)
                    .context("加载 SenseVoice 模型失败(文件损坏或内存不足)")?,
            );
        }
        Ok(self.recognizer.as_ref().expect("just loaded"))
    }
}

impl SttEngine for LocalSenseVoice {
    fn transcribe(&mut self, sample_rate: u32, samples: &[f32]) -> Result<String> {
        let recognizer = self.recognizer()?;
        let stream = recognizer.create_stream();
        stream.accept_waveform(sample_rate as i32, samples);
        recognizer.decode(&stream);
        Ok(stream
            .get_result()
            .map(|result| result.text.trim().to_string())
            .unwrap_or_default())
    }

    fn is_loaded(&self) -> bool {
        self.recognizer.is_some()
    }

    fn preload(&mut self) -> Result<()> {
        self.recognizer().map(|_| ())
    }

    fn release(&mut self) {
        self.recognizer = None;
    }
}

/// f32 采样 → 16-bit PCM WAV 字节。
pub fn encode_wav(sample_rate: u32, samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// 16-bit PCM WAV 字节 → (采样率, 单声道 f32)。只认最常见的 PCM 格式,
/// 浏览器端与 `encode_wav` 都按这个格式产出。
pub fn decode_wav(bytes: &[u8]) -> Result<(u32, Vec<f32>)> {
    anyhow::ensure!(
        bytes.len() >= 44 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "不是 WAV 文件"
    );
    let mut offset = 12;
    let mut channels = 1u16;
    let mut sample_rate = 16_000u32;
    let mut bits = 16u16;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let body = offset + 8;
        if id == b"fmt " {
            anyhow::ensure!(body + 16 <= bytes.len(), "fmt 块截断");
            let format = u16::from_le_bytes(bytes[body..body + 2].try_into().unwrap());
            anyhow::ensure!(format == 1, "只支持 PCM WAV(format={format})");
            channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().unwrap());
            sample_rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
            bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into().unwrap());
        } else if id == b"data" {
            anyhow::ensure!(bits == 16, "只支持 16-bit PCM(bits={bits})");
            let end = (body + size).min(bytes.len());
            let frame = 2 * channels.max(1) as usize;
            let mut samples = Vec::with_capacity((end - body) / frame);
            for chunk in bytes[body..end].chunks_exact(frame) {
                let mut sum = 0.0f32;
                for ch in 0..channels.max(1) as usize {
                    let value = i16::from_le_bytes([chunk[ch * 2], chunk[ch * 2 + 1]]);
                    sum += f32::from(value) / 32768.0;
                }
                samples.push(sum / channels.max(1) as f32);
            }
            return Ok((sample_rate, samples));
        }
        offset = body + size + (size & 1);
    }
    anyhow::bail!("WAV 缺 data 块")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_round_trip() {
        let samples: Vec<f32> = (0..1600).map(|i| (i as f32 / 50.0).sin() * 0.5).collect();
        let bytes = encode_wav(16_000, &samples);
        let (rate, decoded) = decode_wav(&bytes).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(decoded.len(), samples.len());
        assert!((decoded[100] - samples[100]).abs() < 1e-3);
    }
}
