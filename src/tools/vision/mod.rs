pub mod inline;
mod print;
mod reference;
pub(crate) use print::*;
pub(crate) use reference::*;

use super::{ToolRegistry, ToolSpec};
use crate::clipboard::write_image_cache_file;
use crate::config::{AppConfig, PrintImagePluginConfig};
use crate::llm::{ChatMessage, OpenAiCompatibleClient};
use crate::paths::MiyuPaths;
use crate::platform_types::{PlatformContextImageRef, PlatformImageData};
// 工具层只认这个 trait：主体身份、管理员标志、宿主工具放行、按消息取图。
// 依赖 PlatformTurnContext 本身等于把整个平台运行时钉进工具层。
use crate::platform_types::PlatformToolContext;
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;

pub fn register(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    register_analyze: bool,
) {
    if !register_analyze {
        return;
    }
    registry.register(ToolSpec::new(
        "vision_analyze",
        "Analyze an image or a video using the current multimodal model or a configured vision provider. Supports local paths and http(s) URLs. Video formats: mp4, mkv, mov, webm, mpeg; a URL costs far less than a local file, which has to be inlined as base64.",
        json!({
            "type": "object",
            "properties": {
                "image": { "type": "string", "description": "Local path or http(s) URL of an image or a video." },
                "images": { "type": "array", "items": { "type": "string" }, "description": "Several images to analyze in one call (paths or URLs). Overrides image. Videos are analyzed one at a time — pass a single video through `image`." },
                "prompt": { "type": "string", "description": "Question or instruction for image analysis. Defaults to a concise description." }
            },
            "required": [],
            "additionalProperties": false
        }),
        move |args| {
            let config = config.clone();
            let paths = paths.clone();
            async move { analyze_image(args, config, paths).await }
        },
    ));
}

pub fn register_scoped_local(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    allowed_images: Vec<PathBuf>,
) {
    register_scoped(
        registry,
        config,
        paths,
        allowed_images,
        Vec::new(),
        None,
        false,
    );
}

pub fn register_scoped_platform(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    allowed_images: Vec<PathBuf>,
    context_images: Vec<PlatformContextImageRef>,
    platform_context: Arc<dyn PlatformToolContext>,
) {
    let allow_general_access = platform_context.host_tools_allowed();
    register_scoped(
        registry,
        config,
        paths,
        allowed_images,
        context_images,
        Some(platform_context),
        allow_general_access,
    );
}

fn register_scoped(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    allowed_images: Vec<PathBuf>,
    context_images: Vec<PlatformContextImageRef>,
    platform_context: Option<Arc<dyn PlatformToolContext>>,
    allow_general_access: bool,
) {
    let allowed_paths = allowed_images
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect::<Vec<_>>();
    let context_images = context_images
        .into_iter()
        .map(|image| (image.id.clone(), image))
        .collect::<HashMap<_, _>>();
    // Register even with an empty scope: keeping the tool pinned keeps the
    // provider-visible tools array byte-stable across turns (cache prefix).
    // Analysis calls against an empty scope fail with the existing clear
    // "not attached to the current platform turn" style errors.
    let state = Arc::new(ScopedVisionState {
        allowed_paths,
        context_images,
        platform_context,
        allow_general_access,
        resolve_lock: tokio::sync::Mutex::new(()),
        resolved: Mutex::new(HashMap::new()),
        content_images: Mutex::new(HashMap::new()),
        analyses: Mutex::new(HashMap::new()),
        calls: AtomicUsize::new(0),
        fetches: AtomicUsize::new(0),
        total_bytes: AtomicUsize::new(0),
    });
    // 生图的参考图与看图共用同一份作用域:两者都会把图片原样送到第三方,
    // 信任面必须一致(08-17)。只在插件启用时接管,否则保持工具不存在。
    if config.plugins.image_generation.enabled {
        super::image_generation::register_scoped(
            registry,
            config.clone(),
            ReferenceResolver {
                config: config.clone(),
                paths: paths.clone(),
                state: Some(state.clone()),
            },
            state.platform_context.is_some(),
        );
    }
    if !config.plugins.vision.enabled {
        // 只为生图的参考图建作用域:看图插件关着就不注册 vision_analyze。
        return;
    }
    registry.register(ToolSpec::new(
        "vision_analyze",
        "Analyze an image. image can be an image path from this turn's prompt or context_image_N; historical context images are fetched on demand.",
        json!({
            "type": "object",
            "properties": {
                "image": { "type": "string", "description": "A path listed in this turn's image prompt, or a historical image ID such as context_image_1." },
                "images": { "type": "array", "items": { "type": "string" }, "description": "Several images to analyze in one call. Overrides image." },
                "prompt": { "type": "string", "description": "Question or instruction for the image analysis. Defaults to a concise description." }
            },
            "required": [],
            "additionalProperties": false
        }),
        move |args| {
            let config = config.clone();
            let paths = paths.clone();
            let state = state.clone();
            async move { analyze_scoped_image(args, config, paths, state).await }
        },
    ));
    registry.amend_description(
        "vision_analyze",
        if allow_general_access {
            " Historical image IDs from this turn (context_image_N) are fetched on demand; plain local paths and URLs still work as well."
        } else {
            " Only these images may be analyzed: this turn's paths from the current or quoted message, context_image_N IDs explicitly listed in earlier group-chat history, or avatar_url links returned by the group query tools. No other paths or URLs are allowed."
        },
    );
}

/// `images` 数组非空时返回目标列表;否则 None=单图路径。
fn batch_targets(args: &Value) -> Option<Vec<String>> {
    let list = args.get("images")?.as_array()?;
    let targets: Vec<String> = list
        .iter()
        .filter_map(Value::as_str)
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect();
    (!targets.is_empty()).then_some(targets)
}

/// 批内并发上限。视觉供应商单请求秒级,4 路已把 7 张图压进两个批次;
/// 再高容易撞中转限流。
const VISION_BATCH_CONCURRENCY: usize = 4;

type VisionJob = std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>;

/// 每个目标合成单图参数交给 `make_job`,保序有界并发,汇总为分节文本。
/// 单张失败不掀整批,该节记 ERROR(纯文本输出=按成功处理,错误信息模型
/// 自己看得懂)。
async fn run_vision_batch(
    targets: Vec<String>,
    prompt: Option<Value>,
    make_job: impl Fn(Value) -> VisionJob,
) -> Result<String> {
    use futures_util::StreamExt;
    let jobs: Vec<VisionJob> = targets
        .iter()
        .map(|target| {
            let mut sub = json!({ "image": target });
            if let Some(prompt) = &prompt {
                sub["prompt"] = prompt.clone();
            }
            make_job(sub)
        })
        .collect();
    let results: Vec<Result<String>> = futures_util::stream::iter(jobs)
        .buffered(VISION_BATCH_CONCURRENCY)
        .collect()
        .await;
    let sections = targets
        .iter()
        .zip(results)
        .enumerate()
        .map(|(index, (target, result))| match result {
            Ok(analysis) => format!(
                "[Image {}] {}
{}",
                index + 1,
                target,
                analysis.trim()
            ),
            Err(error) => format!(
                "[Image {}] {}
ERROR: {:#}",
                index + 1,
                target,
                error
            ),
        })
        .collect::<Vec<_>>();
    Ok(sections.join(
        "

",
    ))
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn batch_targets_requires_non_empty_array() {
        assert!(batch_targets(&json!({"image": "a.png"})).is_none());
        assert!(batch_targets(&json!({"images": []})).is_none());
        assert!(batch_targets(&json!({"images": ["  "]})).is_none());
        assert_eq!(
            batch_targets(&json!({"images": ["a.png", " b.png "]})).unwrap(),
            vec!["a.png".to_string(), "b.png".to_string()]
        );
    }

    /// 批量输出保序分节;单张失败记 ERROR 不掀整批;prompt 透传给每张。
    #[tokio::test]
    async fn vision_batch_keeps_order_and_isolates_failures() {
        let targets = vec![
            "one.png".to_string(),
            "two.png".to_string(),
            "three.png".to_string(),
        ];
        let output = run_vision_batch(
            targets,
            Some(Value::String("what is it".to_string())),
            |sub| {
                Box::pin(async move {
                    let image = sub["image"].as_str().unwrap().to_string();
                    assert_eq!(sub["prompt"].as_str(), Some("what is it"));
                    if image == "two.png" {
                        bail!("boom")
                    }
                    Ok(format!("desc of {image}"))
                })
            },
        )
        .await
        .unwrap();
        let sections: Vec<&str> = output.split("\n\n").collect();
        assert_eq!(sections.len(), 3);
        assert!(sections[0].starts_with("[Image 1] one.png\ndesc of one.png"));
        assert!(sections[1].starts_with("[Image 2] two.png\nERROR: boom"));
        assert!(sections[2].starts_with("[Image 3] three.png\ndesc of three.png"));
    }
}

async fn analyze_image(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    if let Some(targets) = batch_targets(&args) {
        if let Some(output) = try_inline_targets(&config, &targets)? {
            return Ok(output);
        }
        let prompt = args.get("prompt").cloned();
        return run_vision_batch(targets, prompt, |sub| {
            let config = config.clone();
            let paths = paths.clone();
            Box::pin(async move { analyze_image_one(sub, config, paths).await })
        })
        .await;
    }
    analyze_image_one(args, config, paths).await
}

async fn analyze_image_one(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    let vision = &config.plugins.vision;
    if !vision.enabled {
        bail!("vision plugin is disabled")
    }
    let image = args
        .get("image")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if image.is_empty() {
        bail!("image (or images) is required")
    }
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Describe this image concisely and point out the important details.")
        .trim();
    if let Some(output) = try_inline_targets(&config, std::slice::from_ref(&image.to_string()))? {
        return Ok(output);
    }
    // 视频走独立路由(08-22):OpenRouter 系 video_url 内容块,仅视频能力
    // 模型(如 ox-alpha)接受。
    if let Some(mime) = video_mime(image) {
        let video_url = if image.starts_with("http://") || image.starts_with("https://") {
            image.to_string()
        } else {
            local_video_data_url(image, mime)?
        };
        return analyze_video_url_with_prompt(&config, &paths, &video_url, prompt).await;
    }
    let image_url = if image.starts_with("http://") || image.starts_with("https://") {
        image.to_string()
    } else {
        local_image_data_url(image)?
    };
    analyze_image_url_with_prompt(&config, &paths, &image_url, prompt).await
}

/// 按扩展名识别视频并给出 mime;None=按图片处理。
pub(crate) fn video_mime(value: &str) -> Option<&'static str> {
    let lower = value
        .split('?')
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();
    let ext = lower.rsplit('.').next()?;
    Some(match ext {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        // GLM 官方列的三种格式是 mp4 / mkv / mov;mkv 原先不在表里,会被当图片
        // 走(08-27)。mpeg / webm 保留:别的中转吃这些,GLM 自己会退回明确错误。
        "mpeg" | "mpg" => "video/mpeg",
        "mov" => "video/mov",
        "webm" => "video/webm",
        _ => return None,
    })
}

/// 视频体积上限,对齐 GLM 官方规格(08-27:GLM-5V-Turbo / 4.6V / 4.5V 及其他
/// 多模态模型 200MB;GLM-4V-Plus 另有 20MB 且 ≤30 秒的更紧限制,由服务端自己
/// 回错)。原先卡在 24MB,是按"base64 过中转"定的保守线,把 GLM 能吃的量挡在
/// 门外。
///
/// 本地文件要 base64,体积会 +33% 再叠请求 JSON 外壳;超大文件走 URL 更划算
/// ——官方文档也推荐 URL。超限时指引裁剪而不是静默截断。
const MAX_VIDEO_BYTES: u64 = 200 * 1024 * 1024;

pub(crate) fn local_video_data_url(value: &str, mime: &str) -> Result<String> {
    let path = expand_path(value);
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("failed to stat video {}", path.display()))?;
    if !metadata.is_file() {
        bail!("video path is not a file: {}", path.display())
    }
    if metadata.len() > MAX_VIDEO_BYTES {
        bail!(
            "video is {:.1} MB; the limit is {} MB — trim or compress it first (e.g. ffmpeg -ss/-t or lower the resolution)",
            metadata.len() as f64 / 1024.0 / 1024.0,
            MAX_VIDEO_BYTES / 1024 / 1024
        )
    }
    let bytes = std::fs::read(&path)?;
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

pub async fn analyze_video_url_with_prompt(
    config: &AppConfig,
    paths: &MiyuPaths,
    video_url: &str,
    prompt: &str,
) -> Result<String> {
    let vision = &config.plugins.vision;
    if !vision.enabled {
        bail!("vision plugin is disabled")
    }
    let client = video_client(config, paths)?.with_request_timeouts(
        Duration::from_secs(vision.response_header_timeout_seconds.max(1)),
        Duration::from_secs(vision.stream_idle_timeout_seconds.max(1)),
    );
    let endpoint_count = client.endpoint_count();
    let request = client.chat_stream(
        vec![
            ChatMessage::system(
                "Answer based on the video content; do not make up details you cannot see.",
            ),
            ChatMessage::user_with_video(prompt, video_url.to_string()),
        ],
        Vec::new(),
        |_| Ok(()),
    );
    let result = with_image_timeout(vision_pool_timeout(vision, endpoint_count), request).await?;
    if result.content.trim().is_empty() {
        bail!("video model returned empty response")
    }
    Ok(result.content)
}

/// 视频模型路由:显式 video_provider_id/video_model 优先;否则在启用的多模态
/// 模型里挑 models.dev 标了 video 输入能力的;都没有给出可操作的报错。
fn video_client(config: &AppConfig, paths: &MiyuPaths) -> Result<OpenAiCompatibleClient> {
    let vision = &config.plugins.vision;
    let provider_id = vision.video_provider_id.trim();
    let model = vision.video_model.trim();
    if !provider_id.is_empty() || !model.is_empty() {
        if provider_id.is_empty() || model.is_empty() {
            bail!("plugins.vision.video_provider_id 与 video_model 需同时配置");
        }
        let mut provider = config.provider(Some(provider_id))?.clone();
        provider.default_model = model.to_string();
        if !provider
            .models
            .iter()
            .any(|item| item == &provider.default_model)
        {
            provider.models.push(provider.default_model.clone());
        }
        return OpenAiCompatibleClient::new(&provider, config, paths);
    }
    let choices = config
        .active_multimodal_provider_model_choices()
        .into_iter()
        .filter(|choice| {
            config.model_supports_any_input(&choice.provider_id, &choice.model, &["video"])
        })
        .collect::<Vec<_>>();
    if !choices.is_empty() {
        return OpenAiCompatibleClient::from_choices(config, paths, &choices)
            .map(|client| client.with_request_scope("vision"));
    }
    // 两条路都要写出来。原先只指了 video_provider_id 那条,而更自然的做法是把
    // 支持视频的模型选进多模态池——用户按提示去翻 vision 配置,查了一圈才发现
    // 池子根本是空的(08-27)。
    bail!(
        "no video-capable model available: either add a model whose input modalities include \"video\" to the active multimodal model pool (miyu config → 配置多模态模型), or set plugins.vision.video_provider_id/video_model to one (e.g. glm-5.3-flash, or ox-alpha-free on an OpenRouter-compatible relay)"
    )
}

async fn analyze_scoped_image(
    args: Value,
    config: AppConfig,
    paths: MiyuPaths,
    state: Arc<ScopedVisionState>,
) -> Result<String> {
    if let Some(targets) = batch_targets(&args) {
        let prompt = args.get("prompt").cloned();
        return run_vision_batch(targets, prompt, |sub| {
            let config = config.clone();
            let paths = paths.clone();
            let state = state.clone();
            Box::pin(async move { analyze_scoped_image_one(sub, config, paths, state).await })
        })
        .await;
    }
    analyze_scoped_image_one(args, config, paths, state).await
}

async fn analyze_scoped_image_one(
    args: Value,
    config: AppConfig,
    paths: MiyuPaths,
    state: Arc<ScopedVisionState>,
) -> Result<String> {
    let image = args
        .get("image")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if image.is_empty() {
        bail!("image (or images) is required")
    }
    if state.calls.fetch_add(1, Ordering::AcqRel) >= MAX_SCOPED_VISION_CALLS {
        bail!("vision_analyze call limit reached for the current platform turn")
    }
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Describe this image concisely and point out the important details.")
        .trim();
    if state.context_images.contains_key(image) {
        let resolved = resolve_context_image(&paths, &state, image).await?;
        let cache_key = (resolved.digest.clone(), prompt.to_string());
        if let Some(cached) = state.analyses.lock().unwrap().get(&cache_key).cloned() {
            return Ok(cached);
        }
        if active_text_pool_for_vision(&config).is_some() {
            return Ok(inline::deposit(vec![crate::state::TurnInlineMedia {
                call_id: String::new(),
                seq: 0,
                kind: crate::state::INLINE_MEDIA_KIND_IMAGE.to_string(),
                mime: resolved.image.mime.clone(),
                source: image.to_string(),
                data: Some(resolved.image.data.to_vec()),
            }]));
        }
        let image_url = image_data_url(&resolved.image.mime, &resolved.image.data);
        let result = analyze_image_url_with_prompt(&config, &paths, &image_url, prompt).await?;
        state
            .analyses
            .lock()
            .unwrap()
            .insert(cache_key, result.clone());
        return Ok(result);
    }
    if state.allow_general_access {
        return analyze_image_one(args, config, paths).await;
    }
    if image.starts_with("http://") || image.starts_with("https://") {
        // QQ avatar URLs are built by our own tools from numeric IDs
        // (fixed host, digits-only parameters), so admitting them opens
        // no injection or exfiltration surface.
        if crate::platform_types::is_trusted_avatar_url(image) {
            return analyze_image_one(args, config, paths).await;
        }
        bail!("only images attached to the current platform turn are allowed")
    }
    let image = expand_path(image)
        .canonicalize()
        .context("failed to resolve the requested image")?;
    if !state.allowed_paths.iter().any(|allowed| allowed == &image) {
        bail!("image is not attached to the current platform turn")
    }
    if let Some(output) = try_inline_targets(&config, &[image.display().to_string()])? {
        return Ok(output);
    }
    analyze_local_image_with_prompt(&config, &paths, &image, prompt).await
}

pub async fn analyze_local_image_with_prompt(
    config: &AppConfig,
    paths: &MiyuPaths,
    image: &Path,
    prompt: &str,
) -> Result<String> {
    let image_url = local_image_data_url(&image.display().to_string())?;
    analyze_image_url_with_prompt(config, paths, &image_url, prompt).await
}

pub async fn analyze_image_url_with_prompt(
    config: &AppConfig,
    paths: &MiyuPaths,
    image_url: &str,
    prompt: &str,
) -> Result<String> {
    let vision = &config.plugins.vision;
    if !vision.enabled {
        bail!("vision plugin is disabled")
    }
    let client = vision_client(config, paths)?.with_request_timeouts(
        Duration::from_secs(vision.response_header_timeout_seconds.max(1)),
        Duration::from_secs(vision.stream_idle_timeout_seconds.max(1)),
    );
    let endpoint_count = client.endpoint_count();
    let request = client.chat_stream(
        vec![
            ChatMessage::system(
                "Answer based on the image content; do not make up details you cannot see.",
            ),
            ChatMessage::user_with_image(prompt, image_url.to_string()),
        ],
        Vec::new(),
        |_| Ok(()),
    );
    let result = with_image_timeout(vision_pool_timeout(vision, endpoint_count), request).await?;
    if result.content.trim().is_empty() {
        bail!("vision model returned empty response")
    }
    Ok(result.content)
}

/// 罩在整条故障转移外面的总预算。
///
/// `image_timeout_seconds` 原本是个固定的总超时，可它跟端点数无关。08-18 实测：
/// 60s 预算 / 单端点 15s 响应头超时 = 最多容得下 4 个卡住的端点，排在后面的哪怕
/// 能用也永远轮不到；而且报错会从「5 个端点各自为什么失败」退化成一句
/// 「pool timed out」，把定位所需的信息全丢掉。
///
/// 所以总预算不能小于「所有端点各自超时之和」：按单端点超时 × 端点数兜底，
/// 配置里的值只当下限。单端点那两个超时仍然是真正的保护，一个卡住的端点最多
/// 拖 `response_header_timeout_seconds`。
pub(crate) fn vision_pool_timeout(
    vision: &crate::config::VisionPluginConfig,
    endpoints: usize,
) -> u64 {
    let worst_case = vision
        .response_header_timeout_seconds
        .max(1)
        .saturating_mul(endpoints.max(1) as u64)
        .saturating_add(vision.stream_idle_timeout_seconds);
    vision.image_timeout_seconds.max(worst_case)
}

pub(crate) async fn with_image_timeout<T, F>(timeout_seconds: u64, future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let timeout = Duration::from_secs(timeout_seconds.max(1));
    tokio::time::timeout(timeout, future).await.map_err(|_| {
        anyhow::anyhow!(
            "vision model pool timed out after {} seconds",
            timeout.as_secs()
        )
    })?
}

/// 当前文本模型池自己就能看图时,`vision_analyze` 直接用它。
///
/// `prefer_current_multimodal_model` 此前只管一件事:粘贴进来的图片要不要
/// 内联发给聊天模型。`vision_analyze` 完全没看这个开关——哪怕当前文本模型
/// 自带眼睛,工具照旧把图发给另配的多模态池,既多一次跨模型往返,答案也来
/// 自一个没有对话上下文的模型(08-17 用户报的问题)。
///
/// 要求整池都支持图片输入:池是负载均衡的,只要有一个端点不认图片,这一路
/// 就可能随机落到它头上。
fn active_text_pool_for_vision(
    config: &AppConfig,
) -> Option<Vec<crate::config::ProviderModelChoice>> {
    if !config.plugins.vision.prefer_current_multimodal_model {
        return None;
    }
    let pool = config.active_provider_model_choices();
    let usable = !pool.is_empty()
        && pool.iter().all(|choice| {
            config.model_supports_any_input(&choice.provider_id, &choice.model, &["image"])
        });
    usable.then_some(pool)
}

/// 活跃文本池整池支持某种输入(image/video)。池是负载均衡的,有一个不认
/// 就不能算。
fn active_text_pool_supports(config: &AppConfig, input: &str) -> bool {
    if !config.plugins.vision.prefer_current_multimodal_model {
        return false;
    }
    let pool = config.active_provider_model_choices();
    !pool.is_empty()
        && pool.iter().all(|choice| {
            config.model_supports_any_input(&choice.provider_id, &choice.model, &[input])
        })
}

/// 当前模型自己能看时,不发旁路请求:把媒体寄存给回合循环,返回 inline
/// 标记。任一目标当前池吃不下(视频而池不认视频)就整批退回旁路,别把一
/// 半图给主模型、另一半交给别的模型转述。
///
/// 只做"能不能读到"级别的校验(文件存在、体积在上限内),不解码——解码由
/// 供应商负责,坏图它会明确报错。
fn try_inline_targets(config: &AppConfig, targets: &[String]) -> Result<Option<String>> {
    if !config.plugins.vision.enabled {
        return Ok(None);
    }
    let mut items = Vec::with_capacity(targets.len());
    for target in targets {
        let target = target.trim();
        let remote = target.starts_with("http://") || target.starts_with("https://");
        if let Some(mime) = video_mime(target) {
            if !active_text_pool_supports(config, "video") {
                return Ok(None);
            }
            if !remote {
                // 与旁路同一把尺:超限时指引裁剪,不静默截断。
                local_video_data_url_check(target)?;
            }
            items.push(crate::state::TurnInlineMedia {
                call_id: String::new(),
                seq: 0,
                kind: crate::state::INLINE_MEDIA_KIND_VIDEO.to_string(),
                mime: mime.to_string(),
                source: if remote {
                    target.to_string()
                } else {
                    expand_path(target).display().to_string()
                },
                data: None,
            });
            continue;
        }
        if active_text_pool_for_vision(config).is_none() {
            return Ok(None);
        }
        if remote {
            items.push(crate::state::TurnInlineMedia {
                call_id: String::new(),
                seq: 0,
                kind: crate::state::INLINE_MEDIA_KIND_IMAGE.to_string(),
                mime: String::new(),
                source: target.to_string(),
                data: None,
            });
        } else {
            let (mime, bytes) = local_image_bytes(target)?;
            items.push(crate::state::TurnInlineMedia {
                call_id: String::new(),
                seq: 0,
                kind: crate::state::INLINE_MEDIA_KIND_IMAGE.to_string(),
                mime: mime.to_string(),
                source: expand_path(target).display().to_string(),
                data: Some(bytes),
            });
        }
    }
    Ok(Some(inline::deposit(items)))
}

/// 只校验不读:视频内联时正文由重放方按需从文件读。
fn local_video_data_url_check(value: &str) -> Result<()> {
    let path = expand_path(value);
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("failed to stat video {}", path.display()))?;
    if !metadata.is_file() {
        bail!("video path is not a file: {}", path.display())
    }
    if metadata.len() > MAX_VIDEO_BYTES {
        bail!(
            "video is {:.1} MB; the limit is {} MB — trim or compress it first (e.g. ffmpeg -ss/-t or lower the resolution)",
            metadata.len() as f64 / 1024.0 / 1024.0,
            MAX_VIDEO_BYTES / 1024 / 1024
        )
    }
    Ok(())
}

fn vision_client(config: &AppConfig, paths: &MiyuPaths) -> Result<OpenAiCompatibleClient> {
    // An explicit global vision provider preserves its existing precedence.
    // Platform turns with a conversation override clear that single-provider
    // field in their private config clone, exposing the full routed pool here.
    if config.plugins.vision.vision_provider_id.trim().is_empty() {
        if let Some(text_pool) = active_text_pool_for_vision(config) {
            return OpenAiCompatibleClient::from_choices(config, paths, &text_pool)
                .map(|client| client.with_request_scope("vision"));
        }
        let choices = config
            .active_multimodal_provider_model_choices()
            .into_iter()
            .filter(|choice| {
                config.model_supports_any_input(&choice.provider_id, &choice.model, &["image"])
            })
            .collect::<Vec<_>>();
        if !choices.is_empty() {
            return OpenAiCompatibleClient::from_choices(config, paths, &choices)
                .map(|client| client.with_request_scope("vision"));
        }
    }
    let (provider_id, model) = config.vision_provider_choice()?;
    let mut provider = config.provider(Some(&provider_id))?.clone();
    provider.default_model = model;
    if !provider
        .models
        .iter()
        .any(|item| item == &provider.default_model)
    {
        provider.models.push(provider.default_model.clone());
    }
    OpenAiCompatibleClient::new(&provider, config, paths)
}

pub(crate) fn local_image_bytes(value: &str) -> Result<(&'static str, Vec<u8>)> {
    let path = expand_path(value);
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("failed to stat image {}", path.display()))?;
    if !metadata.is_file() {
        bail!("image path is not a file: {}", path.display())
    }
    if metadata.len() as usize > MAX_IMAGE_BYTES {
        bail!("image too large: {} bytes", metadata.len())
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("failed to read image {}", path.display()))?;
    let mime = mime_from_path(&path)?;
    Ok((mime, bytes))
}

pub(crate) fn local_image_data_url(value: &str) -> Result<String> {
    let (mime, bytes) = local_image_bytes(value)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{mime};base64,{encoded}"))
}

fn expand_path(value: &str) -> PathBuf {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        super::workspace::effective_workdir().join(path)
    }
}

fn mime_from_path(path: &Path) -> Result<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => Ok("image/jpeg"),
        "png" => Ok("image/png"),
        "webp" => Ok("image/webp"),
        "gif" => Ok("image/gif"),
        value => {
            bail!("unsupported image extension: {value}; supported: jpg, jpeg, png, webp, gif")
        }
    }
}

#[cfg(test)]
mod video_route_tests {
    use super::*;

    /// 扩展名分流是视频路由的唯一开关:带查询串的 URL、大小写、图片后缀
    /// 都不能误判。
    #[test]
    fn video_mime_detection_covers_url_and_case() {
        assert_eq!(video_mime("/tmp/a.mp4"), Some("video/mp4"));
        assert_eq!(video_mime("/tmp/A.MOV"), Some("video/mov"));
        assert_eq!(
            video_mime("https://x.com/v.webm?sig=abc"),
            Some("video/webm")
        );
        assert_eq!(video_mime("/tmp/a.png"), None);
        assert_eq!(video_mime("https://x.com/v"), None);
        // GLM 官方列的三种格式必须全认(08-27:mkv 原先漏了,会被当图片走)。
        for (path, mime) in [
            ("/tmp/a.mp4", "video/mp4"),
            ("/tmp/a.mkv", "video/x-matroska"),
            ("/tmp/a.mov", "video/mov"),
        ] {
            assert_eq!(video_mime(path), Some(mime), "GLM 支持的格式: {path}");
        }
    }

    /// 体积上限对齐 GLM 官方规格(200MB);卡在旧的 24MB 会把 GLM 能吃的量挡住。
    #[test]
    fn video_size_cap_matches_the_glm_limit() {
        assert_eq!(MAX_VIDEO_BYTES, 200 * 1024 * 1024);
    }

    /// wire 形态锁定:GLM 官方文档与 OpenRouter/Qwen 系一致,都是
    /// {"type":"video_url","video_url":{"url":…}}(08-27 对过官方 API 文档)。
    #[test]
    fn video_part_serializes_to_openrouter_shape() {
        let message =
            crate::llm::ChatMessage::user_with_video("看看这段", "data:video/mp4;base64,AAAA");
        let json = serde_json::to_value(&message).unwrap();
        let parts = json["content"].as_array().unwrap();
        assert_eq!(parts[1]["type"], "video_url");
        assert_eq!(parts[1]["video_url"]["url"], "data:video/mp4;base64,AAAA");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ActiveProviderModelConfig;
    use crate::platforms::{
        ConversationKind, OutboundMessage, PlatformAdapter, PlatformConversation, SendReceipt,
    };
    use crate::state::StateStore;
    use futures_util::future::BoxFuture;

    struct ContextImageAdapter {
        calls: Arc<AtomicUsize>,
        images: Vec<PlatformImageData>,
    }

    impl PlatformAdapter for ContextImageAdapter {
        fn send<'a>(&'a self, _message: OutboundMessage) -> BoxFuture<'a, Result<SendReceipt>> {
            Box::pin(async { bail!("send is not used in this test") })
        }

        fn bot_display_name<'a>(&'a self) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { Ok("Miyu".to_string()) })
        }

        fn message_images<'a>(
            &'a self,
            _message_id: &'a str,
        ) -> BoxFuture<'a, Result<Vec<PlatformImageData>>> {
            let calls = self.calls.clone();
            let images = self.images.clone();
            Box::pin(async move {
                tokio::task::yield_now().await;
                calls.fetch_add(1, Ordering::AcqRel);
                Ok(images)
            })
        }
    }

    fn test_paths(root: &Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish"),
            bash_hook_file: root.join("bash"),
            zsh_hook_file: root.join("zsh"),
            scripts_dir: root.join("scripts"),
            system_scripts_dir: root.join("system-scripts"),
        }
    }

    /// 平台回合的作用域不能只由看图插件把门:生图的参考图共用同一份作用域,
    /// vision 关、生图开时若不建作用域,generate_image 会留着不受限的解析器。
    #[test]
    fn scoped_registration_binds_image_generation_even_without_vision() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::paths::MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish"),
            bash_hook_file: temp.path().join("bash"),
            zsh_hook_file: temp.path().join("zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: temp.path().join("system-scripts"),
        };
        let mut config = AppConfig::default();
        config.plugins.vision.enabled = false;
        config.plugins.image_generation.enabled = true;

        let mut registry = ToolRegistry::new();
        register_scoped_local(&mut registry, config, paths, Vec::new());
        // 看图插件关着 ⇒ 不注册 vision_analyze,但生图必须换成带作用域的版本。
        assert!(!registry.contains("vision_analyze"));
        assert!(registry.contains("generate_image"));
    }

    /// 当前文本模型自己能看图时就用它,不再绕道另配的多模态池。
    #[test]
    fn vision_uses_the_active_text_pool_when_it_can_see() {
        let mut config = AppConfig::default();
        let provider = config
            .providers
            .iter_mut()
            .find(|provider| !provider.is_builtin_cli_provider())
            .unwrap();
        let provider_id = provider.id.clone();
        provider.model_modalities.insert(
            provider.default_model.clone(),
            vec!["text".to_string(), "image".to_string()],
        );
        provider
            .model_modalities
            .insert("blind-model".to_string(), vec!["text".to_string()]);
        provider.models.push("blind-model".to_string());
        assert!(active_text_pool_for_vision(&config).is_some());

        // 开关关掉就走原路。
        config.plugins.vision.prefer_current_multimodal_model = false;
        assert!(active_text_pool_for_vision(&config).is_none());
        config.plugins.vision.prefer_current_multimodal_model = true;

        // 池里只要混进一个不认图片的端点就不能用:负载均衡会随机落到它。
        config.active_provider_models = Some(vec![
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: config
                    .providers
                    .iter()
                    .find(|provider| !provider.is_builtin_cli_provider())
                    .unwrap()
                    .default_model
                    .clone(),
            },
            ActiveProviderModelConfig {
                provider_id,
                model: "blind-model".to_string(),
            },
        ]);
        assert!(active_text_pool_for_vision(&config).is_none());
    }

    /// 08-18 实测的那次：5 个端点，其中 3 个各卡满 15s，总共 45.9s；固定的
    /// 60s 预算刚好没被撑破。再多一个卡住的端点就会被从中间砍断——排在后面的
    /// 端点哪怕能用也永远轮不到。
    #[test]
    fn the_pool_budget_covers_every_endpoint_timing_out() {
        let mut vision = crate::config::VisionPluginConfig::default();
        vision.response_header_timeout_seconds = 15;
        vision.stream_idle_timeout_seconds = 20;
        vision.image_timeout_seconds = 60;

        // 端点少时，配置里的值仍然说了算
        assert_eq!(vision_pool_timeout(&vision, 1), 60);
        assert_eq!(vision_pool_timeout(&vision, 2), 60);

        // 端点一多，预算跟着涨：5 × 15 + 20 = 95 > 60
        assert_eq!(vision_pool_timeout(&vision, 5), 95);
        // 关键回归：9 个端点全卡住也要够，不能停在 60
        assert_eq!(vision_pool_timeout(&vision, 9), 155);
        assert!(
            vision_pool_timeout(&vision, 9) >= vision.response_header_timeout_seconds * 9,
            "预算必须罩得住每个端点各自超时一次"
        );
    }

    /// 端点数为 0（不该发生）也不能算出 0 秒预算。
    #[test]
    fn the_pool_budget_is_never_zero() {
        let mut vision = crate::config::VisionPluginConfig::default();
        vision.response_header_timeout_seconds = 0;
        vision.stream_idle_timeout_seconds = 0;
        vision.image_timeout_seconds = 0;
        assert!(vision_pool_timeout(&vision, 0) >= 1);
    }

    #[tokio::test]
    async fn image_timeout_cancels_a_stalled_model_pool() {
        let error = with_image_timeout(1, std::future::pending::<Result<()>>())
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "vision model pool timed out after 1 seconds"
        );
    }

    #[tokio::test]
    async fn context_images_reuse_resolved_ids_and_duplicate_content_cache() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let calls = Arc::new(AtomicUsize::new(0));
        let adapter = Arc::new(ContextImageAdapter {
            calls: calls.clone(),
            images: vec![PlatformImageData {
                mime: "image/png".to_string(),
                data: Arc::from(vec![1_u8, 2, 3]),
            }],
        });
        let context = Arc::new(crate::platforms::PlatformTurnContext::new(
            PlatformConversation {
                platform: "onebot".to_string(),
                account_id: "10000".to_string(),
                kind: ConversationKind::Group,
                conversation_id: "20000".to_string(),
            },
            "30000".to_string(),
            "tester".to_string(),
            false,
            AppConfig::default(),
            paths.clone(),
            StateStore::new(&paths).unwrap(),
            adapter,
            Arc::new(crate::platforms::plugins::PlatformPluginRegistry::default()),
        ));
        let source = PlatformContextImageRef {
            id: "context_image_1".to_string(),
            message_id: "90".to_string(),
            image_index: 1,
        };
        let duplicate_source = PlatformContextImageRef {
            id: "context_image_2".to_string(),
            message_id: "91".to_string(),
            image_index: 1,
        };
        let state = ScopedVisionState {
            allowed_paths: Vec::new(),
            context_images: [
                (source.id.clone(), source),
                (duplicate_source.id.clone(), duplicate_source),
            ]
            .into(),
            platform_context: Some(context),
            allow_general_access: false,
            resolve_lock: tokio::sync::Mutex::new(()),
            resolved: Mutex::new(HashMap::new()),
            content_images: Mutex::new(HashMap::new()),
            analyses: Mutex::new(HashMap::new()),
            calls: AtomicUsize::new(0),
            fetches: AtomicUsize::new(0),
            total_bytes: AtomicUsize::new(0),
        };

        let (first, second) = tokio::join!(
            resolve_context_image(&paths, &state, "context_image_1"),
            resolve_context_image(&paths, &state, "context_image_1")
        );
        let first = first.unwrap();
        let second = second.unwrap();
        let duplicate = resolve_context_image(&paths, &state, "context_image_2")
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::Acquire), 2);
        assert_eq!(first.digest, second.digest);
        assert_eq!(first.cache_path, second.cache_path);
        assert_eq!(first.cache_path, duplicate.cache_path);
        assert_eq!(state.total_bytes.load(Ordering::Acquire), 3);
        assert!(first.cache_path.is_file());
        let error = resolve_context_image(&paths, &state, "context_image_999")
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("context image ID is not available"));
        assert_eq!(calls.load(Ordering::Acquire), 2);
    }
    #[test]
    fn inline_short_circuit_only_when_the_text_pool_can_see() {
        let temp = tempfile::tempdir().unwrap();
        let image_path = temp.path().join("dot.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))
            .save(&image_path)
            .unwrap();
        let target = image_path.display().to_string();
        let mut config = AppConfig::default();
        // 池不认图片:退回旁路。
        assert!(try_inline_targets(&config, &[target.clone()])
            .unwrap()
            .is_none());
        let provider = config
            .providers
            .iter_mut()
            .find(|provider| !provider.is_builtin_cli_provider())
            .unwrap();
        provider.model_modalities.insert(
            provider.default_model.clone(),
            vec!["text".to_string(), "image".to_string()],
        );
        // 池认图片:寄存并返回 inline 标记,媒体本体不在结果文本里。
        let output = try_inline_targets(&config, &[target.clone()])
            .unwrap()
            .expect("inline output");
        assert!(inline::inline_reference(&output).is_some());
        assert!(!output.contains("base64"));
        let items = inline::take_from_output(&output);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, crate::state::INLINE_MEDIA_KIND_IMAGE);
        assert_eq!(items[0].mime, "image/png");
        assert_eq!(
            items[0].data.as_deref(),
            Some(std::fs::read(&image_path).unwrap().as_slice())
        );
        // 视频而池不认视频:整批退回旁路,不拆一半给主模型。
        assert!(
            try_inline_targets(&config, &[target, "https://x/y.mp4".to_string()])
                .unwrap()
                .is_none()
        );
        // 关掉偏好开关:一律旁路。
        config.plugins.vision.prefer_current_multimodal_model = false;
        assert!(
            try_inline_targets(&config, &[image_path.display().to_string()])
                .unwrap()
                .is_none()
        );
    }
}
