//! 图片与元数据的校验。
//!
//! 表情由模型分类、由用户上传，两头都不可信。尺寸、像素总量、GIF 帧数与时长都
//! 有上限——一张 20000×20000 的 PNG 解出来就是十几 GB。
//!
//! `static_gif_preview` 取首帧做静态预览：不是所有终端和平台都放得动 GIF。

use crate::tools::memes::*;

pub(crate) const MIN_IMAGE_EDGE: u32 = 32;

pub(crate) const MAX_IMAGE_EDGE: u32 = 4096;

pub(crate) const MAX_IMAGE_PIXELS: u64 = 16_000_000;

pub(crate) const MAX_GIF_FRAMES: usize = 120;

pub(crate) const MAX_GIF_DURATION_MS: u64 = 15_000;

pub(crate) const MAX_NAME_CHARS: usize = 80;

pub(crate) const MAX_DESCRIPTION_CHARS: usize = 500;

pub(crate) const MAX_USAGE_CHARS: usize = 500;


pub(crate) const MAX_TAGS: usize = 16;

pub(crate) const MAX_TAG_CHARS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidatedImageFormat {
    Jpeg,
    Png,
    Gif,
    Webp,
}

impl ValidatedImageFormat {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    pub(crate) fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

pub(crate) struct PreparedImage {
    pub(crate) source: PathBuf,
    pub(crate) hash: String,
    pub(crate) id: String,
    pub(crate) format: ValidatedImageFormat,
}

pub(crate) fn prepare_image(source: &Path, max_bytes: u64) -> Result<PreparedImage> {
    let metadata = std::fs::metadata(source)
        .with_context(|| format!("failed to stat image {}", source.display()))?;
    if !metadata.is_file() {
        bail!("image path is not a file: {}", source.display())
    }
    if metadata.len() > max_bytes {
        bail!("image exceeds the configured meme size limit")
    }
    let bytes = std::fs::read(source)
        .with_context(|| format!("failed to read image {}", source.display()))?;
    let format = validate_image_bytes(&bytes)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    Ok(PreparedImage {
        source: source.to_path_buf(),
        id: format!("sha256:{hash}"),
        hash,
        format,
    })
}

pub(crate) fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() {
        bail!("{key} is required")
    }
    Ok(value)
}

pub(crate) fn string_array(value: Option<&Value>) -> Vec<String> {
    crate::tools::string_list(value)
}

pub(in crate::tools::memes) fn validate_classification(
    classification: &MemeClassification,
) -> Result<()> {
    if !classification.save {
        return Ok(());
    }
    if classification.confidence != 100 {
        bail!("accepted meme classification confidence must be exactly 100")
    }
    if !classification.positive_gates.chat_reaction
        || !classification.positive_gates.emotion_or_meme
        || !classification.positive_gates.reusable
        || !classification.positive_gates.context_independent
        || !classification.positive_gates.persona_fit
        || !classification.positive_gates.meaning_clear
        || !classification.positive_gates.visual_quality
    {
        bail!("accepted meme classification did not pass every positive gate")
    }
    if classification.risk_gates.ordinary_photo
        || classification.risk_gates.informational_content
        || classification.risk_gates.privacy
        || classification.risk_gates.advertisement
        || classification.risk_gates.unsafe_or_abusive
    {
        bail!("accepted meme classification triggered a risk gate")
    }
    validate_text_field("name.zh", &classification.name.zh, 1, MAX_NAME_CHARS)?;
    validate_text_field("name.en", &classification.name.en, 0, MAX_NAME_CHARS)?;
    validate_text_field(
        "description",
        &classification.description,
        1,
        MAX_DESCRIPTION_CHARS,
    )?;
    validate_text_field("usage", &classification.usage, 1, MAX_USAGE_CHARS)?;
    validate_tags(&classification.tags, true)?;
    Ok(())
}

pub(crate) fn validate_tags(tags: &[String], required: bool) -> Result<()> {
    if (required && tags.is_empty()) || tags.len() > MAX_TAGS {
        bail!(
            "tags must contain between {} and {MAX_TAGS} items",
            usize::from(required)
        )
    }
    let mut normalized = std::collections::HashSet::new();
    for tag in tags {
        validate_text_field("tag", tag, 1, MAX_TAG_CHARS)?;
        if tag.chars().any(char::is_whitespace) {
            bail!("tags must be short single tokens")
        }
        if !normalized.insert(tag.to_lowercase()) {
            bail!("tags must be unique")
        }
    }
    Ok(())
}

pub(crate) fn validate_text_field(name: &str, value: &str, min: usize, max: usize) -> Result<()> {
    let trimmed = value.trim();
    let count = trimmed.chars().count();
    if trimmed != value || count < min || count > max || value.chars().any(char::is_control) {
        bail!("{name} must be trimmed, control-free, and contain {min}..={max} characters")
    }
    Ok(())
}

pub(crate) fn validate_image_bytes(bytes: &[u8]) -> Result<ValidatedImageFormat> {
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("detecting image format")?;
    let image_format = reader.format().context("unsupported image format")?;
    let format = match image_format {
        image::ImageFormat::Jpeg => ValidatedImageFormat::Jpeg,
        image::ImageFormat::Png => ValidatedImageFormat::Png,
        image::ImageFormat::Gif => ValidatedImageFormat::Gif,
        image::ImageFormat::WebP => ValidatedImageFormat::Webp,
        _ => bail!("unsupported image format; supported: jpeg, png, gif, webp"),
    };
    let (width, height) = reader
        .into_dimensions()
        .context("decoding image dimensions")?;
    validate_dimensions(width, height)?;
    if format == ValidatedImageFormat::Gif {
        validate_gif(bytes)?;
    } else {
        image::load_from_memory_with_format(bytes, image_format).context("decoding image")?;
    }
    Ok(format)
}

pub(crate) fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if !(MIN_IMAGE_EDGE..=MAX_IMAGE_EDGE).contains(&width)
        || !(MIN_IMAGE_EDGE..=MAX_IMAGE_EDGE).contains(&height)
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        bail!(
            "image dimensions must be {MIN_IMAGE_EDGE}..={MAX_IMAGE_EDGE} per edge and at most {MAX_IMAGE_PIXELS} pixels"
        )
    }
    Ok(())
}

pub(crate) fn validate_gif(bytes: &[u8]) -> Result<()> {
    let decoder = image::codecs::gif::GifDecoder::new(BufReader::new(Cursor::new(bytes)))
        .context("decoding GIF")?;
    let frames = decoder.into_frames();
    let mut frame_count = 0_usize;
    let mut duration_ms = 0_u64;
    for frame in frames {
        let frame = frame.context("decoding GIF frame")?;
        frame_count += 1;
        if frame_count > MAX_GIF_FRAMES {
            bail!("GIF must contain 1..={MAX_GIF_FRAMES} frames")
        }
        validate_dimensions(frame.buffer().width(), frame.buffer().height())?;
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        if denominator == 0 {
            bail!("GIF frame has an invalid delay")
        }
        duration_ms = duration_ms.saturating_add(
            u64::from(numerator).saturating_add(u64::from(denominator) - 1)
                / u64::from(denominator),
        );
        if duration_ms > MAX_GIF_DURATION_MS {
            bail!("GIF duration exceeds 15 seconds")
        }
    }
    if frame_count == 0 {
        bail!("GIF must contain 1..={MAX_GIF_FRAMES} frames")
    }
    Ok(())
}

pub(crate) async fn static_gif_preview(path: &Path) -> Result<tempfile::NamedTempFile> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)
            .with_context(|| format!("opening GIF {}", path.display()))?;
        let decoder = image::codecs::gif::GifDecoder::new(BufReader::new(file))
            .context("decoding GIF preview")?;
        let frame = decoder
            .into_frames()
            .next()
            .transpose()
            .context("decoding first GIF frame")?
            .context("GIF has no frames")?;
        let temp = tempfile::Builder::new().suffix(".png").tempfile()?;
        frame
            .buffer()
            .save_with_format(temp.path(), image::ImageFormat::Png)
            .context("writing static GIF preview")?;
        Ok(temp)
    })
    .await
    .context("GIF preview task failed")?
}

pub(crate) fn meme_print_size(args: &Value, config: &MemesPluginConfig) -> Option<String> {
    let width = args
        .get("width")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(160);
    let height = args
        .get("height")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(80);
    match (width, height) {
        (0, 0) => args
            .get("size")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| configured_meme_size(config)),
        (width, 0) => Some(format!("{width}x")),
        (0, height) => Some(format!("x{height}")),
        (width, height) => Some(format!("{width}x{height}")),
    }
}

pub(crate) fn configured_meme_size(config: &MemesPluginConfig) -> Option<String> {
    let (cols, rows) = crossterm::terminal::size().ok()?;
    let width = ((cols as u32 * config.width_percent as u32) / 100)
        .max(1)
        .min(160);
    let height = ((rows as u32 * config.height_percent as u32) / 100)
        .max(1)
        .min(80);
    Some(format!("{width}x{height}"))
}

pub(crate) fn has_supplied_metadata(args: &Value) -> bool {
    [
        "name_zh",
        "name_en",
        "description",
        "usage",
        "tags",
    ]
    .iter()
    .any(|key| args.get(*key).is_some())
}

pub(crate) fn item_from_args(
    args: &Value,
    id: String,
    file: String,
    mime_type: String,
    animated: bool,
) -> Result<MemeItem> {
    let name = LocalizedName {
        zh: args
            .get("name_zh")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        en: args
            .get("name_en")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
    };
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let usage = args
        .get("usage")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.zh.is_empty() || description.is_empty() || usage.is_empty() {
        bail!("name_zh, description, and usage are required when supplying metadata manually")
    }
    let tags = string_array(args.get("tags"));
    validate_text_field("name.zh", &name.zh, 1, MAX_NAME_CHARS)?;
    validate_text_field("name.en", &name.en, 0, MAX_NAME_CHARS)?;
    validate_text_field("description", &description, 1, MAX_DESCRIPTION_CHARS)?;
    validate_text_field("usage", &usage, 1, MAX_USAGE_CHARS)?;
    validate_tags(&tags, false)?;
    Ok(MemeItem {
        id,
        name,
        file,
        mime_type,
        animated,
        description,
        usage,
        tags,
        origin: None,
    })
}

pub(in crate::tools::memes) fn item_from_classification(
    id: String,
    file: String,
    mime_type: String,
    animated: bool,
    classification: MemeClassification,
    origin: Option<MemeOrigin>,
) -> Result<MemeItem> {
    validate_classification(&classification)?;
    if !classification.save {
        bail!("vision classification rejected the image")
    }
    let item = MemeItem {
        id,
        name: classification.name,
        file,
        mime_type,
        animated,
        description: classification.description,
        usage: classification.usage,
        tags: classification.tags,
        origin,
    };
    Ok(item)
}

pub(crate) fn apply_updates(item: &mut MemeItem, args: &Value) {
    if let Some(value) = args
        .get("name_zh")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        item.name.zh = value.to_string();
    }
    if let Some(value) = args
        .get("name_en")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        item.name.en = value.to_string();
    }
    if let Some(value) = args
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        item.description = value.to_string();
    }
    if let Some(value) = args
        .get("usage")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        item.usage = value.to_string();
    }
    if args.get("tags").is_some() {
        item.tags = string_array(args.get("tags"));
    }
}
