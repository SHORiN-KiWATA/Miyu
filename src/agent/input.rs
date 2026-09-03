//! 用户输入的预处理。
//!
//! `prepare_user_input` 把一条原始输入变成能进模型的消息：剥掉粘贴进来的系统
//! 提醒块（用户可能整段复制了上一轮的输出）、把图片占位符换成真实路径、把上传
//! 的附件转成 image part。
//!
//! 剥离系统提醒是安全边界而非清洁工作：不剥的话，用户粘贴一段伪造的
//! `<system-reminder>` 就等于往提示词里注入指令。

use crate::agent::*;

impl Agent {
    pub(in crate::agent) async fn prepare_user_input(
        &self,
        input: &str,
        images: &[Option<PastedImage>],
    ) -> Result<PreparedUserInput> {
        let input = clean_user_visible_text(input);
        let binary_images = images
            .iter()
            .filter_map(|image| match image {
                Some(PastedImage::Binary(image)) => Some(image),
                _ => None,
            })
            .collect::<Vec<_>>();
        let path_images = images
            .iter()
            .filter_map(|image| match image {
                Some(PastedImage::Path(path)) => Some(path.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let absolute_image_paths =
            resolve_pasted_image_paths(images, &self.paths, self.image_platform.as_deref());
        let binary_paths = images
            .iter()
            .zip(&absolute_image_paths)
            .filter_map(|(image, path)| {
                matches!(image, Some(PastedImage::Binary(_)))
                    .then(|| path.clone())
                    .flatten()
            })
            .collect::<Vec<_>>();
        // v7 Phase 1.3-b: register the scoped vision tool whenever the platform
        // path is active, even with no images this turn. A conditional
        // registration made the tools array appear/disappear between turns,
        // invalidating the provider prefix cache from token 0; an empty scope
        // simply rejects analysis requests with a clear message instead.
        //
        // 生图的参考图与看图共用同一份作用域,所以这一段不能只由 vision 插件
        // 开关把门:vision 关、生图开时,平台回合的 generate_image 会留着不受
        // 限的解析器,不可信用户一句话就能让它把宿主上任意文件当参考图上传。
        if self.tools_enabled
            && (self.config.plugins.vision.enabled || self.config.plugins.image_generation.enabled)
            && self.image_platform.is_some()
        {
            let mut tools = self.tools.lock().unwrap();
            if let Some(platform_context) = self.platform_context.clone() {
                vision::register_scoped_platform(
                    &mut tools,
                    self.config.clone(),
                    self.paths.clone(),
                    binary_paths.iter().map(PathBuf::from).collect(),
                    self.context_images.clone(),
                    platform_context,
                );
            } else if !tools.contains("vision_analyze") {
                vision::register_scoped_local(
                    &mut tools,
                    self.config.clone(),
                    self.paths.clone(),
                    binary_paths.iter().map(PathBuf::from).collect(),
                );
            }
        }
        let vision_tool_available =
            self.tools_enabled && self.tools.lock().unwrap().contains("vision_analyze");
        let input = rewrite_image_placeholders_with_paths(&input, &absolute_image_paths);
        let current_model_supports_vision = self.current_model_supports_vision();
        // 视频先从路径附件里分出来:它和图片不是同一种内容块,能力判定也各管
        // 各的(08-27 用户要求"当附件"传)。扩展名判据与视觉工具同源,免得两边
        // 对"什么算视频"各有一套。
        let (video_paths, image_paths): (Vec<&str>, Vec<&str>) = path_images
            .iter()
            .partition(|path| crate::tools::vision::video_mime(path).is_some());
        // Path 形态的图(shell-hook 跨进程、REPL 粘贴路径)在视觉模型
        // 在场时同样内联直读,和 Binary 对齐;读不出来(不存在/超限/
        // 非图片扩展名)的落进 `unread_image_paths`,由 vision_analyze 提示兜底。
        //
        // 分开记"内联成功"与"没读进来"是有讲究的:提示只该提没读进来的那些。
        // 原先两处提示都不看图有没有已经内联,于是多模态模型明明已经看见图,
        // 同一轮还会收到一句"你可以用 vision_analyze 分析这些图片"——它照做,
        // 就成了用户看到的"多模态模型总去调视觉工具"(08-27 定位)。
        let mut inline_path_urls: Vec<String> = Vec::new();
        let mut unread_image_paths: Vec<&str> = Vec::new();
        for path in &image_paths {
            match current_model_supports_vision
                .then(|| crate::tools::vision::local_image_data_url(path).ok())
                .flatten()
            {
                Some(url) => inline_path_urls.push(url),
                None => unread_image_paths.push(path),
            }
        }
        // 视频同理:当前模型吃视频才内联,否则原样留给 vision_analyze。读不出来
        // (不存在/超 200MB)的也落回提示,理由与图片一致——附件读不了不该让整
        // 轮对话失败。
        let supports_video = self.current_model_supports_video();
        let mut inline_video_urls: Vec<String> = Vec::new();
        let mut unread_video_paths: Vec<&str> = Vec::new();
        for path in &video_paths {
            // 读盘 + base64 挪到阻塞线程:视频上限 200MB,编码后约 +33%,在
            // async worker 上同步做会把这条线程占住好几秒——daemon 同时还在
            // 服务 QQ 和别的会话(08-27 二轮自审)。
            let owned = path.to_string();
            let url = if supports_video {
                tokio::task::spawn_blocking(move || {
                    let mime = crate::tools::vision::video_mime(&owned)?;
                    crate::tools::vision::local_video_data_url(&owned, mime).ok()
                })
                .await
                .ok()
                .flatten()
            } else {
                None
            };
            match url {
                Some(url) => inline_video_urls.push(url),
                None => unread_video_paths.push(path),
            }
        }
        let content = if !binary_images.is_empty() && !current_model_supports_vision {
            self.describe_images_with_vision_provider(&input, &binary_images)
                .await?
        } else {
            input
        };

        let inlined_video = !inline_video_urls.is_empty();
        let has_inline_media = (!binary_images.is_empty() || !inline_path_urls.is_empty())
            && current_model_supports_vision
            || inlined_video;
        let message = if has_inline_media {
            let mut parts = vec![ChatContentPart::Text {
                text: content.clone(),
            }];
            // 图片这一份要**单独**受视觉能力约束。只因为带了视频就走进这个
            // 分支时,当前模型未必吃图:那种情况下 `content` 已经是视觉插件
            // 写好的图片描述,再把原图塞进去等于同一批图处理两遍,还发给了
            // 我们刚判定"不该收图"的模型(08-27 自审)。
            if current_model_supports_vision {
                parts.extend(binary_images.iter().map(|image| ChatContentPart::ImageUrl {
                    image_url: ImageUrlContent {
                        url: image.data_url().to_string(),
                    },
                }));
            }
            parts.extend(
                inline_path_urls
                    .into_iter()
                    .map(|url| ChatContentPart::ImageUrl {
                        image_url: ImageUrlContent { url },
                    }),
            );
            // 视频块只进**本轮请求**。历史存的是 `content` 这段纯文本
            // (`turns.user_content`),所以附件天然不会被后续每一轮反复重发
            // ——图片一直是这个待遇,视频体积大一两个数量级,更不能破例。
            parts.extend(
                inline_video_urls
                    .into_iter()
                    .map(|url| ChatContentPart::VideoUrl {
                        video_url: crate::llm::VideoUrlContent { url },
                    }),
            );
            ChatMessage::user_parts(parts)
        } else {
            ChatMessage::plain("user", &content)
        };

        let mut hints = Vec::new();
        if !binary_paths.is_empty() {
            let source = self
                .image_platform_label
                .as_deref()
                .or(self.image_platform.as_deref())
                .map(|platform| format!("通过 {platform} 发送"))
                .unwrap_or_else(|| "粘贴".to_string());
            // 图已经内联给模型时不再邀请它调工具:它自己看得见,再调一次是白
            // 烧一次旁路请求。临时文件路径仍然留着——生图参考图、artifact 这些
            // 工具要靠它定位(08-27)。
            let invite_vision_tool = vision_tool_available && !current_model_supports_vision;
            let tool_hint = if invite_vision_tool {
                "\n你可以使用 vision_analyze 工具对此图片进行更详细的分析。"
            } else {
                ""
            };
            let hint = if binary_paths.len() == 1 {
                format!(
                    "用户{source}了 1 张图片，已保存到临时文件：{}{}",
                    binary_paths[0], tool_hint
                )
            } else {
                let list = binary_paths
                    .iter()
                    .enumerate()
                    .map(|(index, path)| format!("  [Image {}] {}", index + 1, path))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "用户{source}了 {} 张图片，已保存到临时文件：\n{}{}",
                    binary_paths.len(),
                    list,
                    if invite_vision_tool {
                        "\n你可以使用 vision_analyze 工具对这些图片进行更详细的分析。"
                    } else {
                        ""
                    }
                )
            };
            hints.push(ChatMessage::turn_context(hint));
        }
        if !unread_image_paths.is_empty() && vision_tool_available {
            let list = unread_image_paths
                .iter()
                .enumerate()
                .map(|(index, path)| format!("  [Image {}] {}", index + 1, path))
                .collect::<Vec<_>>()
                .join("\n");
            hints.push(ChatMessage::turn_context(format!(
                "用户粘贴了 {} 张本地图片路径：\n{}\n你可以使用 vision_analyze 工具读取并分析这些图片。",
                unread_image_paths.len(),
                list
            )));
        }
        // 视频没能内联时才提示走工具:已经内联的那些模型自己看得见,再叫它去
        // 调工具就是白烧一次旁路请求(而且工具那条路还要另外挑一个吃视频的
        // 模型)。
        if !unread_video_paths.is_empty() && vision_tool_available {
            let list = unread_video_paths
                .iter()
                .enumerate()
                .map(|(index, path)| format!("  [Video {}] {}", index + 1, path))
                .collect::<Vec<_>>()
                .join("\n");
            hints.push(ChatMessage::turn_context(format!(
                "用户粘贴了 {} 个本地视频路径：\n{}\n你可以使用 vision_analyze 工具读取并分析这些视频。",
                unread_video_paths.len(),
                list
            )));
        }
        if !self.context_images.is_empty() && !vision_tool_available {
            tracing::warn!(
                target: "miyu::qq",
                refs = self.context_images.len(),
                tools_enabled = self.tools_enabled,
                "{}",
                crate::i18n::text(
                    "context image refs are available but vision_analyze is not registered; the block was not emitted",
                    "有历史图片引用但未注册 vision_analyze,<context-images> 块未发出"
                )
            );
        }
        if !self.context_images.is_empty() && vision_tool_available {
            let ids = self
                .context_images
                .iter()
                .map(|image| image.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            // 用法说明是常量,已随 <qq-context-images> 进 system 提示词
            // (08-17;实测一条 780K token 的群聊请求里这句重复 579 次、
            // 共 139,867 字符)。每轮只留真正会变的 ID 列表。
            hints.push(ChatMessage::turn_context(format!(
                "<context-images>{ids}</context-images>"
            )));
        }

        Ok(PreparedUserInput {
            content,
            message,
            hints,
        })
    }

    /// 当前模型看不了图时的退路:把媒体块里的图片交给视觉插件转述,换成
    /// 一条 text 块(同样落库重放)。
    pub(in crate::agent) async fn describe_inline_media(
        &self,
        items: Vec<crate::state::TurnInlineMedia>,
    ) -> Result<Vec<crate::state::TurnInlineMedia>> {
        let images = items
            .iter()
            .filter(|item| item.kind == crate::state::INLINE_MEDIA_KIND_IMAGE)
            .filter_map(|item| {
                item.data
                    .clone()
                    .map(|data| ClipboardImage::new(item.mime.clone(), data))
            })
            .collect::<Vec<_>>();
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let refs = images.iter().collect::<Vec<_>>();
        let description = self.describe_images_with_vision_provider("", &refs).await?;
        if description.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![crate::state::TurnInlineMedia {
            call_id: String::new(),
            seq: 0,
            kind: crate::state::INLINE_MEDIA_KIND_TEXT.to_string(),
            mime: "text/plain".to_string(),
            source: String::new(),
            data: Some(description.into_bytes()),
        }])
    }

    pub(in crate::agent) fn uploaded_attachment_image_parts(
        &self,
        attachments: &[crate::state::UserAttachment],
    ) -> Vec<ChatContentPart> {
        attachments
            .iter()
            .filter(|attachment| attachment.kind == "image")
            .filter_map(|attachment| {
                self.state
                    .load_user_attachment(&attachment.attachment_id)
                    .ok()
                    .flatten()
            })
            .map(|attachment| ChatContentPart::ImageUrl {
                image_url: ImageUrlContent {
                    url: ClipboardImage::new(attachment.attachment.mime, attachment.bytes)
                        .data_url()
                        .to_string(),
                },
            })
            .collect()
    }

    pub(in crate::agent) fn queued_prompt_images(
        &self,
        prompt: &QueuedPrompt,
    ) -> Result<Vec<Option<PastedImage>>> {
        let mut images = queued_prompt_images(prompt)?;
        for attachment in &prompt.uploaded_attachments {
            if attachment.kind != "image" {
                continue;
            }
            if let Some(data) = self.state.load_user_attachment(&attachment.attachment_id)? {
                images.push(Some(PastedImage::Binary(ClipboardImage::new(
                    data.attachment.mime,
                    data.bytes,
                ))));
            }
        }
        Ok(images)
    }
}
