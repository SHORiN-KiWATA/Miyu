//! 交给视觉模型看图。
//!
//! 只在当前文本模型池吃不下图片时才走这条路（判定见 [`super::images`]）：单独
//! 请求一个视觉模型，把描述文本回填进对话。对模型来说它看到的是一段文字描述，
//! 不是图片。

use crate::agent::*;

impl Agent {
    pub(in crate::agent) fn current_model_supports_vision(&self) -> bool {
        should_use_active_text_pool_for_images(&self.config)
    }

    /// 当前对话模型能否直接看视频。不看 `prefer_current_multimodal_model`
    /// ——那个开关管的是"图片优先走当前模型还是走视觉插件",而视频没有第二
    /// 条内联通路:当前模型不吃视频时只能退回 `vision_analyze` 的旁路请求。
    pub(in crate::agent) fn current_model_supports_video(&self) -> bool {
        crate::agent::images::active_text_pool_supports_video(&self.config)
    }

    /// 当前对话模型能否直接读 PDF。理由同视频:PDF 没有"视觉旁路"这条第二
    /// 通路——`vision_analyze` 只认图片和视频——所以不吃就是不内联。
    pub(in crate::agent) fn current_model_supports_pdf(&self) -> bool {
        crate::agent::images::active_text_pool_supports_pdf(&self.config)
    }

    pub(in crate::agent) async fn describe_images_with_vision_provider(
        &self,
        input: &str,
        images: &[&ClipboardImage],
    ) -> Result<String> {
        let vision_cfg = &self.config.plugins.vision;
        if !vision_cfg.enabled {
            bail!(
                "{}",
                crate::i18n::text(
                    "the active text model cannot read images and the vision plugin is disabled",
                    "当前文本模型无法读取图片，并且视觉插件已禁用"
                )
            );
        }
        let strict_pool = self
            .config
            .active_multimodal_provider_models
            .as_ref()
            .is_some_and(|pool| !pool.is_empty());
        let mut descriptions = Vec::new();
        for (i, img) in images.iter().enumerate() {
            let prompt = if input.trim().is_empty() {
                "Describe this image concisely and point out the important details.".to_string()
            } else {
                format!("User message: {input}\n\nAnswer based on the image content or describe the image; do not make up details you cannot see.")
            };
            match vision::analyze_image_url_with_prompt(
                &self.config,
                &self.paths,
                img.data_url(),
                &prompt,
            )
            .await
            {
                Ok(desc) => {
                    descriptions.push(format!("[Image {} 的描述]\n{}", i + 1, desc.trim()));
                }
                Err(error) if strict_pool => {
                    return Err(error).with_context(|| {
                        format!(
                            "configured multimodal model pool failed for image {}",
                            i + 1
                        )
                    });
                }
                Err(error) => {
                    descriptions.push(format!("[Image {} 识图失败: {}]", i + 1, error));
                }
            }
        }
        let combined = descriptions.join("\n\n");
        if input.trim().is_empty() {
            Ok(combined)
        } else {
            Ok(format!("{input}\n\n{combined}"))
        }
    }
}
