//! `PlatformAdapter` 的 OneBot 实现。
//!
//! 平台无关的上层只认这个 trait；QQ 特有的东西（CQ 码、群成员角色、回复目标前
//! 缀）都被挡在这一层之内。
//!
//! `prepend_response_target` 在群里给回复加 @：私聊不加，群里加，而且只加一次
//! ——上层不知道这个区别，也不该知道。

use crate::platforms::onebot::*;

pub(in crate::platforms::onebot) struct OneBotAdapter {
    pub(in crate::platforms::onebot) conn: ConnectionHandle,
    pub(in crate::platforms::onebot) registry: Arc<Mutex<ConnectionRegistry>>,
    pub(in crate::platforms::onebot) http: reqwest::Client,
    pub(in crate::platforms::onebot) self_id: i64,
    pub(in crate::platforms::onebot) target: Target,
    pub(in crate::platforms::onebot) max_reply_chars: usize,
    pub(in crate::platforms::onebot) file_store_lock: Arc<tokio::sync::Mutex<()>>,
}

pub(in crate::platforms::onebot) fn prepend_response_target(
    segments: &mut Vec<Value>,
    target: &ResponseTarget,
) {
    let mut index = 0;
    if target.quote && !target.message_id.is_empty() {
        segments.insert(
            index,
            json!({ "type": "reply", "data": { "id": target.message_id } }),
        );
        index += 1;
    }
    let mut seen = HashSet::new();
    let mut mention_user_ids = Vec::new();
    if target.mention && !target.user_id.is_empty() {
        seen.insert(target.user_id.as_str());
        mention_user_ids.push(target.user_id.as_str());
    }
    for user_id in &target.explicit_mention_user_ids {
        let user_id = user_id.trim();
        if !user_id.is_empty() && seen.insert(user_id) {
            mention_user_ids.push(user_id);
        }
    }
    for user_id in mention_user_ids {
        segments.insert(index, json!({ "type": "at", "data": { "qq": user_id } }));
        index += 1;
        // OneBot renders an `at` segment adjacent to the following text.
        // Keep the generated target readable on clients that do not add
        // visual separation themselves.
        segments.insert(index, text_segment(" "));
        index += 1;
    }
}

impl PlatformAdapter for OneBotAdapter {
    fn send<'a>(&'a self, message: OutboundMessage) -> BoxFuture<'a, Result<SendReceipt>> {
        Box::pin(async move { self.send_message(message).await })
    }

    fn bot_display_name<'a>(&'a self) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let conn = self.connection();
            if let Some(name) = conn.bot_name.lock().unwrap().clone() {
                return Ok(name);
            }
            let data = conn.call_api("get_login_info", json!({})).await?;
            let name = data
                .get("nickname")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or("Bot")
                .to_string();
            *conn.bot_name.lock().unwrap() = Some(name.clone());
            Ok(name)
        })
    }

    fn message_images<'a>(
        &'a self,
        message_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PlatformImageData>>> {
        Box::pin(async move {
            let data = get_message_data(
                &self.connection(),
                message_id,
                QUOTED_MESSAGE_LOOKUP_TIMEOUT,
            )
            .await?;
            let info = parse_message_info(&data, self.self_id)
                .context("OneBot image message metadata is unavailable")?;
            let expected_kind = match self.target {
                Target::Private { .. } => ConversationKind::Private,
                Target::Group { .. } => ConversationKind::Group,
            };
            let expected_id = self.target.conversation_id().to_string();
            if info.conversation_kind != Some(expected_kind)
                || info.conversation_id.as_deref() != Some(expected_id.as_str())
            {
                bail!("the requested image message belongs to another conversation")
            }
            let mut images = Vec::new();
            let mut total_bytes = 0usize;
            let sources =
                ordered_message_image_sources(data.get("message"), data.get("raw_message"));
            for source in sources {
                let remaining = MAX_INBOUND_IMAGE_TOTAL_BYTES.saturating_sub(total_bytes);
                if remaining == 0 {
                    break;
                }
                let maximum = MAX_INBOUND_IMAGE_BYTES.min(remaining);
                let media = match source {
                    OrderedMessageImageSource::Media(media) => media,
                    OrderedMessageImageSource::File(file) => {
                        let Ok(data) = self
                            .connection()
                            .call_api_with_timeout(
                                "get_image",
                                json!({ "file": file }),
                                QUOTED_MESSAGE_LOOKUP_TIMEOUT,
                            )
                            .await
                        else {
                            continue;
                        };
                        let mut parsed = InboundMessage::default();
                        if !append_resolved_quoted_image(&mut parsed, &data) {
                            continue;
                        }
                        let Some(media) = parsed.images.into_iter().next() else {
                            continue;
                        };
                        media
                    }
                };
                let bytes = match media {
                    MediaRef::Bytes(bytes) if bytes.len() <= maximum => bytes,
                    MediaRef::Bytes(_) => continue,
                    MediaRef::Url(url) => {
                        match download_capped(&self.http, &url, maximum, IMAGE_DOWNLOAD_TIMEOUT)
                            .await
                        {
                            Ok((bytes, _)) => bytes,
                            Err(error) => {
                                tracing::debug!(%error, "{}", t("meme collector image download failed", "表情包收集器图片下载失败"));
                                continue;
                            }
                        }
                    }
                };
                total_bytes += bytes.len();
                images.push(PlatformImageData {
                    mime: sniff_image_mime(&bytes).to_string(),
                    data: Arc::from(bytes),
                });
            }
            Ok(images)
        })
    }

    fn bot_send_availability<'a>(&'a self) -> BoxFuture<'a, Result<BotSendAvailability>> {
        Box::pin(async move {
            let Target::Group { group_id } = self.target else {
                return Ok(BotSendAvailability::Available);
            };
            let key = (self.self_id, group_id);
            let now = Instant::now();
            if let Some(availability) = group_mute_cache().lock().unwrap().get(key, now) {
                return Ok(availability);
            }

            let result = self
                .connection()
                .call_api_with_timeout(
                    "get_group_member_info",
                    json!({
                        "group_id": group_id,
                        "user_id": self.self_id,
                        // 这一条**必须** no_cache:NapCat 的成员信息缓存在提前
                        // 解禁之后仍然带着旧的 shut_up_timestamp,信它就会以为
                        // 自己还在禁言里(09-11 实录见 GROUP_MUTE_MUTED_TTL)。
                        // 频率由禁言缓存兜着,不会变成每条消息一次查询。
                        "no_cache": true,
                    }),
                    GROUP_MUTE_LOOKUP_TIMEOUT,
                )
                .await;
            let now_unix = unix_now();
            let (availability, ttl) = match result {
                Ok(data) => match group_member_mute_until(&data) {
                    Some(muted_until) if muted_until > now_unix => (
                        BotSendAvailability::Muted,
                        Duration::from_secs((muted_until - now_unix) as u64)
                            .min(GROUP_MUTE_MUTED_TTL),
                    ),
                    Some(_) => (BotSendAvailability::Available, GROUP_MUTE_AVAILABLE_TTL),
                    None => (BotSendAvailability::Unknown, GROUP_MUTE_UNKNOWN_TTL),
                },
                Err(error) => {
                    tracing::debug!(
                        target: "miyu::qq",
                        error = %error,
                        self_id = self.self_id,
                        group_id,
                        "{}",
                        t("OneBot bot mute-state lookup failed", "OneBot 机器人禁言状态查询失败")
                    );
                    (BotSendAvailability::Unknown, GROUP_MUTE_UNKNOWN_TTL)
                }
            };
            group_mute_cache()
                .lock()
                .unwrap()
                .insert(key, availability, ttl, now);
            Ok(availability)
        })
    }

    fn set_message_reaction<'a>(
        &'a self,
        message_id: &'a str,
        reaction_id: &'a str,
        active: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if message_id.trim().is_empty() || reaction_id.trim().is_empty() {
                bail!("message_id and reaction_id are required");
            }
            self.connection()
                .call_api(
                    "set_msg_emoji_like",
                    json!({
                        "message_id": onebot_id_value(message_id),
                        "emoji_id": onebot_id_value(reaction_id),
                        "emoji_type": "1",
                        "set": active,
                    }),
                )
                .await?;
            Ok(())
        })
    }

    fn message_info<'a>(
        &'a self,
        message_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PlatformMessageInfo>>> {
        Box::pin(async move {
            if message_id.trim().is_empty() {
                return Ok(None);
            }
            let data = get_message_data(&self.connection(), message_id, API_CALL_TIMEOUT).await?;
            Ok(parse_message_info(&data, self.self_id))
        })
    }

    fn fetch_platform_file<'a>(
        &'a self,
        file_ref: &'a PlatformContextFileRef,
        paths: &'a crate::paths::MiyuPaths,
    ) -> BoxFuture<'a, Result<PlatformFileDownload>> {
        Box::pin(async move { self.fetch_platform_file_impl(file_ref, paths).await })
    }

    fn group_members<'a>(&'a self) -> BoxFuture<'a, Result<Vec<PlatformGroupMember>>> {
        Box::pin(async move {
            let Target::Group { group_id } = self.target else {
                bail!("group member lookup requires a group conversation");
            };
            let data = self
                .connection()
                .call_api(
                    "get_group_member_list",
                    json!({ "group_id": group_id, "no_cache": false }),
                )
                .await?;
            let members = data
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|member| parse_group_member(member, group_id))
                .collect();
            Ok(members)
        })
    }

    fn group_member<'a>(
        &'a self,
        user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PlatformGroupMember>>> {
        self.group_member_lookup(user_id, false)
    }

    fn group_member_fresh<'a>(
        &'a self,
        user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PlatformGroupMember>>> {
        self.group_member_lookup(user_id, true)
    }

    fn bot_group_role<'a>(&'a self) -> BoxFuture<'a, Result<BotGroupRole>> {
        Box::pin(async move {
            let Target::Group { group_id } = self.target else {
                return Ok(BotGroupRole::Unknown);
            };
            let key = (self.self_id, group_id);
            let now = Instant::now();
            if let Some(role) = group_role_cache().lock().unwrap().get(key, now) {
                return Ok(role);
            }
            let data = self
                .connection()
                .call_api(
                    "get_group_member_info",
                    json!({
                        "group_id": group_id,
                        "user_id": self.self_id,
                        "no_cache": false,
                    }),
                )
                .await?;
            let role = match data.get("role").and_then(Value::as_str) {
                Some("owner") => BotGroupRole::Owner,
                Some("admin") => BotGroupRole::Admin,
                Some("member") => BotGroupRole::Member,
                _ => BotGroupRole::Unknown,
            };
            group_role_cache().lock().unwrap().insert(key, role, now);
            Ok(role)
        })
    }

    fn delete_message<'a>(&'a self, message_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let message_id = message_id.trim();
            if message_id.is_empty() || message_id.len() > MAX_ONEBOT_ID_BYTES {
                bail!("invalid OneBot message id");
            }
            let numeric = message_id
                .parse::<i32>()
                .context("OneBot message id is outside the supported numeric range")?;
            self.connection()
                .call_api("delete_msg", json!({ "message_id": numeric }))
                .await?;
            Ok(())
        })
    }

    fn set_group_ban<'a>(
        &'a self,
        user_id: &'a str,
        duration_seconds: u64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Target::Group { group_id } = self.target else {
                bail!("group ban requires a group conversation");
            };
            self.connection()
                .call_api(
                    "set_group_ban",
                    json!({
                        "group_id": group_id,
                        "user_id": onebot_id_value(user_id),
                        "duration": duration_seconds,
                    }),
                )
                .await?;
            Ok(())
        })
    }

    fn set_group_kick<'a>(
        &'a self,
        user_id: &'a str,
        reject_add_request: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Target::Group { group_id } = self.target else {
                bail!("group kick requires a group conversation");
            };
            self.connection()
                .call_api(
                    "set_group_kick",
                    json!({
                        "group_id": group_id,
                        "user_id": onebot_id_value(user_id),
                        "reject_add_request": reject_add_request,
                    }),
                )
                .await?;
            Ok(())
        })
    }

    fn set_group_special_title<'a>(
        &'a self,
        user_id: &'a str,
        special_title: &'a str,
        duration_seconds: i64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Target::Group { group_id } = self.target else {
                bail!("group title requires a group conversation");
            };
            self.connection()
                .call_api(
                    "set_group_special_title",
                    json!({
                        "group_id": group_id,
                        "user_id": onebot_id_value(user_id),
                        "special_title": special_title,
                        "duration": duration_seconds,
                    }),
                )
                .await?;
            Ok(())
        })
    }
}

impl OneBotAdapter {
    /// `no_cache` asks NapCat to re-read the roster from the server instead of
    /// answering from its own copy, which can still list members who left.
    pub(in crate::platforms::onebot) fn group_member_lookup<'a>(
        &'a self,
        user_id: &'a str,
        no_cache: bool,
    ) -> BoxFuture<'a, Result<Option<PlatformGroupMember>>> {
        Box::pin(async move {
            let Target::Group { group_id } = self.target else {
                bail!("group member lookup requires a group conversation");
            };
            if user_id.trim().is_empty() {
                return Ok(None);
            }
            let data = self
                .connection()
                .call_api(
                    "get_group_member_info",
                    json!({
                        "group_id": group_id,
                        "user_id": onebot_id_value(user_id),
                        "no_cache": no_cache,
                    }),
                )
                .await?;
            Ok(parse_group_member(&data, group_id))
        })
    }

    pub(in crate::platforms::onebot) fn connection(&self) -> ConnectionHandle {
        self.registry
            .lock()
            .unwrap()
            .handle(self.self_id)
            .unwrap_or_else(|| self.conn.clone())
    }

    /// 找到文件在哪:段自带的 url 最省;否则先按会话类型问群/私聊文件直链,
    /// 再用 NapCat 通用的 `get_file` 兜底——视频段的 file_id 不是群文件 id,
    /// 只有 `get_file` 认得(09-04)。返回按优先级排好的候选来源。
    /// 问桥文件在哪:先按会话类型要群/私聊文件直链,再用 NapCat 通用的
    /// `get_file` 兜底——视频段的 file_id 不是群文件 id,只有 `get_file`
    /// 认得(09-04)。返回按优先级排好的候选来源。
    async fn resolve_platform_file_sources(
        &self,
        file_ref: &PlatformContextFileRef,
    ) -> Result<Vec<PlatformFileSource>> {
        let (action, params) = match self.target {
            Target::Group { group_id } => (
                "get_group_file_url",
                json!({ "group_id": group_id, "file_id": file_ref.file_id }),
            ),
            Target::Private { user_id } => (
                "get_private_file_url",
                json!({ "user_id": user_id, "file_id": file_ref.file_id }),
            ),
        };
        let mut errors = Vec::new();
        match self
            .connection()
            .call_api_with_timeout(action, params, FILE_DOWNLOAD_TIMEOUT)
            .await
        {
            Ok(data) => {
                let sources = parse_platform_file_sources(&data);
                if !sources.is_empty() {
                    return Ok(sources);
                }
                errors.push(format!("{action} returned no usable url"));
            }
            Err(error) => errors.push(format!("{action}: {error:#}")),
        }
        match self
            .connection()
            .call_api_with_timeout(
                "get_file",
                json!({ "file_id": file_ref.file_id, "file": file_ref.file_id }),
                FILE_DOWNLOAD_TIMEOUT,
            )
            .await
        {
            Ok(data) => {
                let sources = parse_platform_file_sources(&data);
                if !sources.is_empty() {
                    return Ok(sources);
                }
                errors.push("get_file returned no usable url, path, or base64".to_string());
            }
            Err(error) => errors.push(format!("get_file: {error:#}")),
        }
        bail!(
            "the platform file URL API returned no usable url ({})",
            errors.join("; ")
        )
    }

    /// 依次尝试候选来源,第一个成功的落进缓存。`Ok(None)` = 全部不可达且
    /// 没有实质错误(比如只有别的机器上的本地路径)。
    async fn fetch_platform_file_from_sources(
        &self,
        sources: Vec<PlatformFileSource>,
        file_ref: &PlatformContextFileRef,
        cache_dir: &std::path::Path,
        max_bytes: usize,
    ) -> Result<Option<PathBuf>> {
        let mut last_error = None;
        for source in sources {
            let attempt = match &source {
                PlatformFileSource::Url(url) => {
                    download_platform_file_capped(
                        &self.http,
                        url,
                        cache_dir,
                        &file_ref.file_name,
                        max_bytes,
                        FILE_DOWNLOAD_TIMEOUT,
                    )
                    .await
                }
                PlatformFileSource::LocalPath(local) => {
                    // 桥不在同一台机器上时路径自然不存在,静默退回下一形态。
                    if tokio::fs::metadata(local).await.is_err() {
                        continue;
                    }
                    copy_platform_file_capped(local, cache_dir, &file_ref.file_name, max_bytes)
                        .await
                }
                PlatformFileSource::Bytes(bytes) => {
                    if bytes.len() > max_bytes {
                        Err(anyhow::anyhow!(
                            "the file is larger than the {}MB limit",
                            max_bytes / 1024 / 1024
                        ))
                    } else {
                        save_platform_file(cache_dir, &file_ref.file_name, bytes).await
                    }
                }
            };
            match attempt {
                Ok(path) => return Ok(Some(path)),
                Err(error) => last_error = Some(error),
            }
        }
        match last_error {
            Some(error) => Err(error),
            None => Ok(None),
        }
    }

    pub(in crate::platforms::onebot) async fn fetch_platform_file_impl(
        &self,
        file_ref: &PlatformContextFileRef,
        paths: &crate::paths::MiyuPaths,
    ) -> Result<PlatformFileDownload> {
        migrate_legacy_platform_file_cache(paths).await;
        let max_bytes = platform_file_byte_limit(&file_ref.file_name);
        // 配额预检不持锁(尽力而为,明显已满时省掉一次下载);下载本身也
        // 不持锁——此前全局锁跨最长 60s 的下载持有,一个慢文件会让所有
        // 群所有用户的文件下载排队。
        ensure_platform_file_capacity(
            &paths.cache_dir,
            max_bytes as u64,
            PLATFORM_FILE_STORAGE_BYTES,
            PLATFORM_FILE_STORAGE_ENTRIES,
            PLATFORM_FILE_TTL,
        )
        .await?;
        // 段自带的直链最省,先试;直链失效(CDN 链接过期)再问桥。没有真正的
        // file_id(占位符里 file_id 就是 url 本身)就没法问,直接报直链的错。
        let mut fetched = None;
        let mut direct_error = None;
        if let Some(url) = file_ref.url.as_deref() {
            match self
                .fetch_platform_file_from_sources(
                    vec![PlatformFileSource::Url(url.to_string())],
                    file_ref,
                    &paths.cache_dir,
                    max_bytes,
                )
                .await
            {
                Ok(path) => fetched = path,
                Err(error) => direct_error = Some(error),
            }
        }
        let has_provider_id = !file_ref.file_id.is_empty() && !file_ref.file_id.starts_with("http");
        if fetched.is_none() && has_provider_id {
            if let Some(error) = &direct_error {
                tracing::debug!(
                    target: "miyu::qq",
                    error = %error,
                    file = %file_ref.id,
                    "direct platform file url failed; asking the bridge"
                );
            }
            let sources = self.resolve_platform_file_sources(file_ref).await?;
            fetched = self
                .fetch_platform_file_from_sources(sources, file_ref, &paths.cache_dir, max_bytes)
                .await?;
        }
        let Some(path) = fetched else {
            return Err(direct_error.unwrap_or_else(|| {
                anyhow::anyhow!("the platform file is not reachable from this machine")
            }));
        };
        // 落位复查才持锁:锁只护"清点→裁决"窗口。复查针对既成事实
        // (存量已含刚写入的文件),并发下载一起冲破配额时超额者自删。
        let verdict = {
            let _file_store_guard = self.file_store_lock.lock().await;
            scan_platform_file_storage(&paths.cache_dir, PLATFORM_FILE_TTL)
                .await
                .map(|(bytes, count)| {
                    count <= PLATFORM_FILE_STORAGE_ENTRIES && bytes <= PLATFORM_FILE_STORAGE_BYTES
                })
        };
        match verdict {
            Ok(true) => {}
            Ok(false) => {
                let _ = tokio::fs::remove_file(&path).await;
                bail!("platform file storage quota is full");
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&path).await;
                return Err(error);
            }
        }
        let size = tokio::fs::metadata(&path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        Ok(PlatformFileDownload {
            path,
            name: file_ref.file_name.clone(),
            size,
        })
    }
}
