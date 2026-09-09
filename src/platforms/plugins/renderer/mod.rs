mod fonts;
mod layout;
mod markdown;
mod paint;
mod table_width;
mod worker;
use fonts::*;
use layout::*;
use markdown::*;
use paint::*;
use table_width::*;
// main 要判断自己是不是被当成渲染子进程拉起来的
use worker::*;
pub(crate) use worker::{renderer_worker_requested, run_renderer_worker};

use anyhow::{anyhow, bail, Context, Result};
use cosmic_text::{
    Align as TextAlign, Attrs, Buffer, Color, Family, FontSystem, LayoutGlyph, Metrics, Shaping,
    Style as FontStyle, SwashCache, Weight, Wrap,
};
use fontdb::Database as FontDatabase;
use image::codecs::png::PngEncoder;
use image::{ColorType, ImageEncoder, Pixel as _, Rgba, RgbaImage};
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RenderConfig {
    pub(crate) theme: String,
    pub(crate) max_height: u32,
    pub(crate) font_size: u32,
    pub(crate) code_font_size: u32,
    pub(crate) padding: u32,
    pub(crate) font: String,
    pub(crate) title_font: String,
    pub(crate) code_font: String,
    pub(crate) emoji_font: String,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            theme: "paper".to_string(),
            max_height: 2600,
            font_size: 36,
            code_font_size: 30,
            padding: 64,
            font: String::new(),
            title_font: String::new(),
            code_font: String::new(),
            emoji_font: String::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RenderedImage {
    pub(crate) mime: String,
    pub(crate) png: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone)]
pub(crate) struct MarkdownImageRenderer {
    worker: Arc<Mutex<WorkerSlot>>,
}

struct RendererState {
    font_system: FontSystem,
    swash_cache: SwashCache,
    resolved_fonts: HashMap<String, Option<String>>,
    emoji_font_path: PathBuf,
    emoji_loaded: bool,
}

impl MarkdownImageRenderer {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            worker: Arc::new(Mutex::new(WorkerSlot::default())),
        })
    }

    pub(crate) async fn render(
        &self,
        markdown: &str,
        config: &RenderConfig,
    ) -> Result<Vec<RenderedImage>> {
        validate_markdown(markdown)?;
        #[cfg(test)]
        {
            render_in_process_for_test(markdown, config)
        }
        #[cfg(not(test))]
        {
            self.render_with_worker(markdown, config).await
        }
    }

    #[cfg(not(test))]
    async fn render_with_worker(
        &self,
        markdown: &str,
        config: &RenderConfig,
    ) -> Result<Vec<RenderedImage>> {
        let request = RenderRequest {
            markdown: markdown.to_string(),
            config: config.clone(),
        };
        let mut slot = self.worker.lock().await;
        slot.cancel_idle_timer();

        for attempt in 0..2 {
            let mut worker = match slot.process.take() {
                Some(worker) => worker,
                None => WorkerProcess::spawn().await?,
            };
            let result =
                tokio::time::timeout(RENDER_TIMEOUT, exchange_with_worker(&mut worker, &request))
                    .await;
            match result {
                Ok(Ok(images)) => {
                    self.recycle_worker(&mut slot, worker);
                    return Ok(images);
                }
                Ok(Err(WorkerExchangeError::Render(message))) => {
                    self.recycle_worker(&mut slot, worker);
                    return Err(anyhow!(
                        "long-image renderer rejected the request: {message}"
                    ));
                }
                Ok(Err(WorkerExchangeError::Transport(error))) => {
                    stop_worker(worker).await;
                    if attempt == 1 {
                        return Err(error)
                            .context("long-image renderer worker communication failed");
                    }
                }
                Err(_) => {
                    stop_worker(worker).await;
                    bail!(
                        "long-image renderer exceeded its {}-second timeout",
                        RENDER_TIMEOUT.as_secs()
                    );
                }
            }
        }
        unreachable!("renderer worker retry loop always returns")
    }

    #[cfg(not(test))]
    fn recycle_worker(&self, slot: &mut WorkerSlot, worker: WorkerProcess) {
        slot.process = Some(worker);
        slot.generation = slot.generation.wrapping_add(1);
        let generation = slot.generation;
        let weak_slot = Arc::downgrade(&self.worker);
        slot.idle_task = Some(tokio::spawn(async move {
            tokio::time::sleep(WORKER_IDLE_TIMEOUT).await;
            let Some(shared_slot) = weak_slot.upgrade() else {
                return;
            };
            let mut slot = shared_slot.lock().await;
            if slot.generation != generation {
                return;
            }
            if let Some(worker) = slot.process.take() {
                stop_worker(worker).await;
            }
            slot.idle_task.take();
        }));
    }
}

#[cfg(test)]
fn render_in_process_for_test(
    markdown: &str,
    raw_config: &RenderConfig,
) -> Result<Vec<RenderedImage>> {
    static RENDERER: std::sync::OnceLock<std::sync::Mutex<RendererState>> =
        std::sync::OnceLock::new();
    let renderer = RENDERER.get_or_init(|| std::sync::Mutex::new(RendererState::new().unwrap()));
    let mut renderer = renderer.lock().unwrap();
    validate_markdown(markdown)?;
    let config = NormalizedConfig::new(raw_config);
    let blocks = collect_blocks(markdown);
    let palette = Palette::for_theme(&config.theme);
    renderer.render(blocks, &config, palette, markdown_contains_emoji(markdown))
}

impl RendererState {
    fn new() -> Result<Self> {
        Self::from_font_dir(&renderer_fonts_dir()?)
    }

    fn from_font_dir(font_dir: &std::path::Path) -> Result<Self> {
        let mut database = FontDatabase::new();
        let cjk_font = font_dir.join(CJK_FONT_FILE);
        database
            .load_font_file(&cjk_font)
            .with_context(|| format!("loading renderer font {}", cjk_font.display()))?;
        if database.faces().next().is_none() {
            bail!("renderer font {} contains no faces", cjk_font.display());
        }
        // 等宽代码字体是可选资产:旧资产包没有它时回落正文字体(不报错),
        // 装了新包立即生效。
        let code_font = font_dir.join(CODE_FONT_FILE);
        if code_font.is_file() {
            if let Err(error) = database.load_font_file(&code_font) {
                tracing::warn!(
                    path = %code_font.display(),
                    %error,
                    "loading the renderer code font failed; code falls back to the body font"
                );
            }
        } else {
            tracing::info!(
                path = %code_font.display(),
                "renderer code font is missing; code falls back to the body font"
            );
        }
        database.set_sans_serif_family(DEFAULT_BODY_FONT);
        database.set_monospace_family(DEFAULT_CODE_FONT);
        Ok(Self {
            font_system: FontSystem::new_with_locale_and_db("zh-CN".to_string(), database),
            swash_cache: SwashCache::new(),
            resolved_fonts: HashMap::new(),
            emoji_font_path: font_dir.join(EMOJI_FONT_FILE),
            emoji_loaded: false,
        })
    }
}

impl RendererState {
    fn render(
        &mut self,
        blocks: Vec<Block>,
        config: &NormalizedConfig,
        palette: Palette,
        needs_emoji: bool,
    ) -> Result<Vec<RenderedImage>> {
        if self.swash_cache.image_cache.len() > MAX_CACHED_GLYPHS {
            self.swash_cache.image_cache.clear();
        }
        let fonts = self.resolve_config_fonts(config, needs_emoji)?;
        let layouts = layout_blocks(&mut self.font_system, blocks, config, palette, &fonts)?;
        let columns = plan_balanced_columns(&layouts, config)?;
        let rendered = render_pages(
            &mut self.font_system,
            &mut self.swash_cache,
            &layouts,
            &columns,
            config,
            palette,
        );
        if self.swash_cache.image_cache.len() > MAX_CACHED_GLYPHS {
            self.swash_cache.image_cache.clear();
        }
        rendered
    }

    fn resolve_config_fonts(
        &mut self,
        config: &NormalizedConfig,
        needs_emoji: bool,
    ) -> Result<ResolvedFonts> {
        let body = self
            .resolve_font(&config.font)
            .or_else(|| Some(DEFAULT_BODY_FONT.to_string()));
        let title = if config.title_font.trim().is_empty() {
            body.clone()
        } else {
            self.resolve_font(&config.title_font)
        };
        let emoji = if needs_emoji {
            let configured = config.emoji_font.trim();
            if configured.is_empty() || configured.eq_ignore_ascii_case(DEFAULT_EMOJI_FONT) {
                self.ensure_bundled_emoji_font()?;
                Some(DEFAULT_EMOJI_FONT.to_string())
            } else if let Some(font) = self.resolve_font(configured) {
                Some(font)
            } else {
                self.ensure_bundled_emoji_font()?;
                Some(DEFAULT_EMOJI_FONT.to_string())
            }
        } else {
            None
        };
        Ok(ResolvedFonts {
            body,
            title,
            code: self
                .resolve_font(&config.code_font)
                .or_else(|| Some(DEFAULT_CODE_FONT.to_string())),
            emoji,
        })
    }

    fn ensure_bundled_emoji_font(&mut self) -> Result<()> {
        if self.emoji_loaded {
            return Ok(());
        }

        let previous_faces = self.font_system.db().faces().count();
        self.font_system
            .db_mut()
            .load_font_file(&self.emoji_font_path)
            .with_context(|| {
                format!(
                    "loading renderer Emoji font {}",
                    self.emoji_font_path.display()
                )
            })?;
        let has_emoji_family = self
            .font_system
            .db()
            .faces()
            .skip(previous_faces)
            .any(|face| {
                face.families
                    .iter()
                    .any(|(family, _)| family.eq_ignore_ascii_case(DEFAULT_EMOJI_FONT))
            });
        if !has_emoji_family {
            bail!(
                "renderer Emoji font {} does not contain the {DEFAULT_EMOJI_FONT} family",
                self.emoji_font_path.display()
            );
        }
        self.emoji_loaded = true;
        Ok(())
    }

    fn resolve_font(&mut self, configured: &str) -> Option<String> {
        let configured = configured.trim();
        if configured.is_empty() {
            return None;
        }
        let path = PathBuf::from(configured);
        if !path.is_file() {
            let bundled_family = self.font_system.db().faces().any(|face| {
                face.families
                    .iter()
                    .any(|(family, _)| family.eq_ignore_ascii_case(configured))
            });
            if !bundled_family {
                tracing::warn!(
                    font = configured,
                    "{}",
                    crate::i18n::text(
                        "long-image renderer font is not a bundled family or readable file; using the default font",
                        "长图渲染器字体不是内置字体族或可读文件；使用默认字体"
                    )
                );
                return None;
            }
            return Some(configured.to_string());
        }
        let path = path.canonicalize().unwrap_or(path);
        let cache_key = path.to_string_lossy().into_owned();
        if let Some(cached) = self.resolved_fonts.get(&cache_key) {
            return cached.clone();
        }
        if self.resolved_fonts.len() >= MAX_CUSTOM_FONT_FILES {
            tracing::warn!(
                font = %path.display(),
                limit = MAX_CUSTOM_FONT_FILES,
                "{}",
                crate::i18n::text(
                    "long-image renderer custom font limit reached; using the default font",
                    "长图渲染器已达到自定义字体上限；使用默认字体"
                )
            );
            return None;
        }

        let previous_faces = self.font_system.db().faces().count();
        let resolved = self
            .font_system
            .db_mut()
            .load_font_file(&path)
            .ok()
            .and_then(|()| {
                self.font_system
                    .db()
                    .faces()
                    .skip(previous_faces)
                    .find_map(|face| face.families.first().map(|(name, _)| name.clone()))
            });
        self.resolved_fonts.insert(cache_key, resolved.clone());
        resolved
    }
}

#[cfg(test)]
mod tests;
