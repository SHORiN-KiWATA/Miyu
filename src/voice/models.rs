//! 语音模型的就位管理:下载、解压、路径解析。
//!
//! 三个模型固定放在 `state_dir/models/` 下,版本号写死在目录名里,
//! 升级模型 = 换这里的常量,旧目录不复用。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const RELEASE_BASE: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download";

pub const KWS_DIR: &str = "sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01";
pub const SENSE_VOICE_DIR: &str = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17";
pub const VAD_FILE: &str = "silero_vad.onnx";

/// 下载体积提示,给确认对话框用。
pub const DOWNLOAD_SIZE_HINT: &str = "约 190MB";

pub struct KwsPaths {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
}

pub struct SenseVoicePaths {
    pub model: PathBuf,
    pub tokens: PathBuf,
}

pub fn kws_paths(models_dir: &Path) -> KwsPaths {
    let dir = models_dir.join(KWS_DIR);
    KwsPaths {
        encoder: dir.join("encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
        decoder: dir.join("decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
        joiner: dir.join("joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
        tokens: dir.join("tokens.txt"),
    }
}

pub fn sense_voice_paths(models_dir: &Path) -> SenseVoicePaths {
    let dir = models_dir.join(SENSE_VOICE_DIR);
    SenseVoicePaths {
        model: dir.join("model.int8.onnx"),
        tokens: dir.join("tokens.txt"),
    }
}

pub fn vad_path(models_dir: &Path) -> PathBuf {
    models_dir.join(VAD_FILE)
}

/// 三个模型是否都已就位。
pub fn models_ready(models_dir: &Path) -> bool {
    let kws = kws_paths(models_dir);
    let sense = sense_voice_paths(models_dir);
    [
        &kws.encoder,
        &kws.decoder,
        &kws.joiner,
        &kws.tokens,
        &sense.model,
        &sense.tokens,
        &vad_path(models_dir),
    ]
    .iter()
    .all(|path| path.is_file())
}

/// 下载缺失的模型。阻塞调用,progress 每个阶段回调一次(给 TUI/CLI
/// 打进度行)。已就位的部分跳过,失败时不留半成品目录。
pub fn ensure_models(models_dir: &Path, progress: &mut dyn FnMut(&str)) -> Result<()> {
    std::fs::create_dir_all(models_dir)
        .with_context(|| format!("创建模型目录 {}", models_dir.display()))?;

    let vad = vad_path(models_dir);
    if !vad.is_file() {
        progress("下载 VAD 模型 (0.6MB)…");
        download_file(&format!("{RELEASE_BASE}/asr-models/{VAD_FILE}"), &vad)?;
    }
    if !models_dir.join(KWS_DIR).is_dir() {
        progress("下载唤醒词模型 (31MB)…");
        download_and_unpack(
            &format!("{RELEASE_BASE}/kws-models/{KWS_DIR}.tar.bz2"),
            models_dir,
            KWS_DIR,
        )?;
    }
    if !models_dir.join(SENSE_VOICE_DIR).is_dir() {
        progress("下载 SenseVoice 识别模型 (156MB)…");
        download_and_unpack(
            &format!("{RELEASE_BASE}/asr-models/{SENSE_VOICE_DIR}.tar.bz2"),
            models_dir,
            SENSE_VOICE_DIR,
        )?;
    }
    if !models_ready(models_dir) {
        bail!(
            "模型下载后校验失败:关键文件缺失,请删除 {} 后重试",
            models_dir.display()
        );
    }
    Ok(())
}

fn download_file(url: &str, target: &Path) -> Result<()> {
    let temporary = target.with_extension("part");
    let mut response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()?
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .with_context(|| format!("下载 {url}"))?;
    let mut file = std::fs::File::create(&temporary)?;
    std::io::copy(&mut response, &mut file)
        .with_context(|| format!("写入 {}", temporary.display()))?;
    std::fs::rename(&temporary, target)?;
    Ok(())
}

fn download_and_unpack(url: &str, models_dir: &Path, expect_dir: &str) -> Result<()> {
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(1800))
        .build()?
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .with_context(|| format!("下载 {url}"))?;
    let decoder = bzip2::read::BzDecoder::new(response);
    let mut archive = tar::Archive::new(decoder);
    if let Err(error) = archive.unpack(models_dir) {
        // 解压中途失败不留半成品,否则下次启动误判"已就位"。
        let _ = std::fs::remove_dir_all(models_dir.join(expect_dir));
        return Err(error).with_context(|| format!("解压 {url}"));
    }
    if !models_dir.join(expect_dir).is_dir() {
        bail!("{url} 解压后没有出现预期目录 {expect_dir}");
    }
    Ok(())
}
