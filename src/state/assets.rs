//! 图片、artifact 与用户附件。
//!
//! 这一层比库多做一件事：**文件系统与库的一致性**。`recover_journal_assets` 和
//! `reconcile_managed_artifacts_for_turn` 处理的都是「库里有记录但文件没了」或
//! 反过来的情况——两个存储没有事务，崩在中间就会不一致。

use crate::state::*;

impl StateStore {
    pub fn save_image_asset(
        &self,
        turn_id: &str,
        tool_id: Option<&str>,
        path: &Path,
        alt: &str,
    ) -> Result<ImageAsset> {
        const MAX_STORED_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("reading image metadata: {}", path.display()))?;
        if !metadata.is_file() {
            bail!("image path is not a file: {}", path.display());
        }
        if metadata.len() > MAX_STORED_IMAGE_BYTES {
            bail!("image exceeds the 20 MiB WebUI storage limit");
        }
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading image for WebUI: {}", path.display()))?;
        let reader = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .context("detecting image format")?;
        let format = reader.format().context("unsupported image format")?;
        let (width, height) = reader
            .into_dimensions()
            .context("reading image dimensions")?;
        if width == 0
            || height == 0
            || width > 40_000
            || height > 40_000
            || u64::from(width) * u64::from(height) > 40_000_000
        {
            bail!("image dimensions are outside the WebUI safety limit");
        }
        let fallback_alt = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image");
        let alt = if alt.trim().is_empty() {
            fallback_alt
        } else {
            alt.trim()
        }
        .chars()
        .filter(|character| !character.is_control())
        .take(500)
        .collect::<String>();
        let asset = ImageAsset {
            asset_id: format!(
                "img_{:032x}_{:016x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0),
                rand::random::<u64>()
            ),
            turn_id: turn_id.to_string(),
            tool_id: tool_id.map(str::to_string),
            mime: format.to_mime_type().to_string(),
            width,
            height,
            alt,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        self.conv_db.insert_image_asset(&asset, &bytes)?;
        Ok(asset)
    }

    pub fn load_image_assets(&self) -> Result<Vec<ImageAsset>> {
        self.conv_db.load_image_assets(&self.session())
    }

    pub fn load_image_asset(&self, asset_id: &str) -> Result<Option<ImageAssetData>> {
        self.conv_db.load_image_asset(asset_id)
    }

    pub fn save_artifact_asset(
        &self,
        turn_id: &str,
        tool_id: Option<&str>,
        path: &Path,
        title: &str,
    ) -> Result<ArtifactAsset> {
        const MAX_ARTIFACT_BYTES: u64 = 20 * 1024 * 1024;
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolving artifact path: {}", path.display()))?;
        let metadata = std::fs::metadata(&canonical)
            .with_context(|| format!("reading artifact metadata: {}", canonical.display()))?;
        if !metadata.is_file() {
            bail!("artifact path is not a file: {}", canonical.display());
        }
        if metadata.len() == 0 || metadata.len() > MAX_ARTIFACT_BYTES {
            bail!("artifact must be between 1 byte and 20 MiB");
        }
        let bytes = std::fs::read(&canonical)
            .with_context(|| format!("reading artifact: {}", canonical.display()))?;
        let fallback_name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact");
        let requested_name = if title.trim().is_empty() {
            fallback_name
        } else {
            title.trim()
        };
        let file_name = requested_name
            .chars()
            .filter(|character| !character.is_control() && !matches!(character, '/' | '\\'))
            .take(180)
            .collect::<String>();
        let file_name = if file_name.trim().is_empty() {
            "artifact".to_string()
        } else {
            file_name
        };
        let (mime, kind) = artifact_media_type(&canonical);
        let source_key = canonical.to_string_lossy().to_string();
        let session_id = self.session();
        let managed_session_dir = self.artifacts_dir.join(session_id.as_ref());
        let identity_scope = if canonical.starts_with(&managed_session_dir) {
            session_id.as_ref()
        } else {
            turn_id
        };
        let hash = blake3::hash(format!("{identity_scope}\0{source_key}").as_bytes());
        let now = chrono::Utc::now().to_rfc3339();
        let asset = ArtifactAsset {
            asset_id: format!("art_{}", &hash.to_hex()[..32]),
            turn_id: turn_id.to_string(),
            tool_id: tool_id.map(str::to_string),
            source_key,
            file_name,
            mime: mime.to_string(),
            kind: kind.to_string(),
            size_bytes: bytes.len() as u64,
            created_at: now.clone(),
            updated_at: now,
        };
        self.conv_db.upsert_artifact_asset(&asset, &bytes)?;
        Ok(asset)
    }

    pub fn load_artifact_assets(&self) -> Result<Vec<ArtifactAsset>> {
        self.conv_db.load_artifact_assets(&self.session())
    }

    pub fn load_artifact_asset(&self, asset_id: &str) -> Result<Option<ArtifactAssetData>> {
        self.conv_db.load_artifact_asset(asset_id)
    }

    /// 记下某次工具调用之后追加给模型的媒体块,供历史重放逐字节回灌。
    pub fn save_turn_inline_media(&self, turn_id: &str, items: &[TurnInlineMedia]) -> Result<()> {
        self.conv_db.insert_turn_inline_media(turn_id, items)
    }

    pub fn load_turn_inline_media(&self, turn_id: &str) -> Result<Vec<TurnInlineMedia>> {
        self.conv_db.load_turn_inline_media(turn_id)
    }

    pub fn save_user_attachment(&self, attachment: &UserAttachment, data: &[u8]) -> Result<()> {
        self.conv_db
            .insert_user_attachment(&self.session(), attachment, data)
    }

    /// 登记一个已经写到 `user_attachment_path()` 的落盘附件。
    pub fn save_user_attachment_file(&self, attachment: &UserAttachment) -> Result<()> {
        self.conv_db
            .insert_user_attachment_file(&self.session(), attachment)
    }

    /// 落盘附件的目标目录(`<state>/attachments/<id>`),上传前由调用方创建。
    pub fn user_attachment_dir(&self, attachment_id: &str) -> std::path::PathBuf {
        self.conv_db.attachment_dir(attachment_id)
    }

    /// 落盘附件的目标文件路径。
    pub fn user_attachment_path(&self, attachment: &UserAttachment) -> std::path::PathBuf {
        self.conv_db.attachment_path(attachment)
    }

    pub fn load_user_attachment(&self, attachment_id: &str) -> Result<Option<UserAttachmentData>> {
        self.conv_db
            .load_user_attachment(&self.session(), attachment_id)
    }

    pub fn load_user_attachment_by_id(
        &self,
        attachment_id: &str,
    ) -> Result<Option<UserAttachmentData>> {
        self.conv_db.load_user_attachment_by_id(attachment_id)
    }

    pub fn load_user_attachment_data_for_turn(
        &self,
        turn_id: &str,
    ) -> Result<Vec<UserAttachmentData>> {
        self.conv_db
            .load_user_attachment_data_for_turn(&self.session(), turn_id)
    }

    pub fn load_user_attachment_data_for_prompt(
        &self,
        prompt_id: &str,
    ) -> Result<Vec<UserAttachmentData>> {
        self.conv_db
            .load_user_attachment_data_for_prompt(&self.session(), prompt_id)
    }

    pub fn load_staged_user_attachments(
        &self,
        attachment_ids: &[String],
    ) -> Result<Vec<UserAttachmentData>> {
        self.conv_db
            .load_user_attachments(&self.session(), attachment_ids)
    }

    pub fn reserve_user_attachments(&self, attachment_ids: &[String], run_id: &str) -> Result<()> {
        self.conv_db
            .reserve_user_attachments(&self.session(), attachment_ids, run_id)
    }

    pub fn release_user_attachments_for_run(&self, run_id: &str) -> Result<usize> {
        self.conv_db.release_user_attachments_for_run(run_id)
    }

    pub fn delete_staged_user_attachment(&self, attachment_id: &str) -> Result<bool> {
        self.conv_db
            .delete_staged_user_attachment(&self.session(), attachment_id)
    }

    pub fn purge_stale_user_attachments(&self) -> Result<usize> {
        self.conv_db.purge_stale_user_attachments()
    }

    pub(crate) fn remove_artifact_session_dirs(&self, session_ids: &[String]) -> Result<()> {
        for session_id in session_ids {
            self.remove_artifact_session_dir(session_id)?;
        }
        Ok(())
    }

    pub(crate) fn remove_artifact_session_dir(&self, session_id: &str) -> Result<()> {
        use std::path::Component;

        let mut components = Path::new(session_id).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            anyhow::bail!("invalid session id for Artifact workspace cleanup");
        }
        let path = self.artifacts_dir.join(session_id);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || metadata.is_file() {
            std::fs::remove_file(path)?;
        } else {
            std::fs::remove_dir_all(path)?;
        }
        Ok(())
    }

    pub(crate) fn recover_journal_assets(&self, session_id: &str, turn_id: &str) -> Result<()> {
        let Some(turn) = self
            .conv_db
            .load_turns(session_id)?
            .into_iter()
            .find(|turn| turn.turn_id == turn_id)
        else {
            return Ok(());
        };
        if turn.journal_events.is_empty() {
            return Ok(());
        }
        let mut images = self
            .conv_db
            .load_image_assets(session_id)?
            .into_iter()
            .filter(|asset| asset.turn_id == turn_id)
            .collect::<Vec<_>>();
        let mut artifacts = self
            .conv_db
            .load_artifact_assets(session_id)?
            .into_iter()
            .filter(|asset| asset.turn_id == turn_id)
            .collect::<Vec<_>>();
        for event in &turn.journal_events {
            let kind = event.kind.as_str();
            if kind != "image" && kind != "artifact" {
                continue;
            }
            let Some(payload) = event
                .text_payload
                .as_deref()
                .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
            else {
                continue;
            };
            let Some(raw_path) = payload.get("path").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let path = PathBuf::from(raw_path);
            if !path.is_file() {
                continue;
            }
            if kind == "image" {
                let alt = payload
                    .get("alt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if images.iter().any(|asset| {
                    asset.tool_id.as_deref() == event.call_id.as_deref() && asset.alt == alt
                }) {
                    continue;
                }
                match self.save_image_asset(turn_id, event.call_id.as_deref(), &path, alt) {
                    Ok(asset) => images.push(asset),
                    Err(error) => tracing::warn!(
                        turn_id,
                        path = %path.display(),
                        error = %error,
                        "failed to recover an interrupted image asset"
                    ),
                }
            } else {
                let Ok(source_key) = path
                    .canonicalize()
                    .map(|path| path.to_string_lossy().into_owned())
                else {
                    continue;
                };
                if artifacts.iter().any(|asset| asset.source_key == source_key) {
                    continue;
                }
                let title = payload
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                match self.save_artifact_asset(turn_id, event.call_id.as_deref(), &path, title) {
                    Ok(asset) => artifacts.push(asset),
                    Err(error) => tracing::warn!(
                        turn_id,
                        path = %path.display(),
                        error = %error,
                        "failed to recover an interrupted Artifact asset"
                    ),
                }
            }
        }
        Ok(())
    }

    pub(crate) fn reconcile_managed_artifacts_for_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<()> {
        use std::path::Component;

        let mut components = Path::new(session_id).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            bail!("invalid session id for Artifact workspace recovery");
        }
        let restored = self.conv_db.load_artifact_asset_data_for_turn(turn_id)?;
        let session_dir = self.artifacts_dir.join(session_id);
        match std::fs::symlink_metadata(&session_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!(
                    "Artifact recovery path is not a directory: {}",
                    session_dir.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && restored.is_empty() => {
                return Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&session_dir)?;
            }
            Err(error) => return Err(error.into()),
        }
        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o700))?;
        let canonical_dir = session_dir.canonicalize()?;
        let managed_target = |source_key: &str| -> Option<PathBuf> {
            let source = Path::new(source_key);
            let file_name = source.file_name()?;
            let parent = source.parent()?.canonicalize().ok()?;
            (parent == canonical_dir).then(|| canonical_dir.join(file_name))
        };

        let keep = self
            .conv_db
            .load_artifact_assets(session_id)?
            .into_iter()
            .filter_map(|asset| managed_target(&asset.source_key))
            .collect::<HashSet<_>>();
        for artifact in restored {
            let Some(target) = managed_target(&artifact.asset.source_key) else {
                continue;
            };
            let mut temp = tempfile::NamedTempFile::new_in(&canonical_dir)?;
            temp.as_file_mut()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
            temp.write_all(&artifact.bytes)?;
            temp.as_file_mut().sync_all()?;
            temp.persist(&target)
                .map_err(|error| error.error)
                .with_context(|| format!("restoring Artifact file: {}", target.display()))?;
        }
        for entry in std::fs::read_dir(&canonical_dir)? {
            let entry = entry?;
            let path = entry.path();
            if keep.contains(&path) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || metadata.is_file() {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}
