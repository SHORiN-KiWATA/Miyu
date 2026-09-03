//! WebUI 文件分享：StateStore 包装层。
//!
//! 库只存元数据（`conversation_db/shared_files.rs`），这一层负责文件系统侧：
//! reference 模式记录原路径 + size/mtime 指纹供下载时校验；snapshot 模式把
//! 文件复制进 `data/shared/{share_id}/`，复制前做磁盘余量检查。

use crate::state::*;

/// 快照复制后磁盘至少要剩这么多，防止把盘写满。
const SNAPSHOT_FREE_MARGIN_BYTES: u64 = 1024 * 1024 * 1024;

/// 扩展名 → (MIME, 前端预览分类)。清单外回退 `application/octet-stream` +
/// `other`：未知类型永远按附件下载，不内联，安全性不降级。
pub fn shared_file_mime_kind(path: &Path) -> (&'static str, &'static str) {
    let Some(extension) = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return ("application/octet-stream", "other");
    };
    match extension.as_str() {
        "mp4" | "m4v" => ("video/mp4", "video"),
        "webm" => ("video/webm", "video"),
        "mov" => ("video/quicktime", "video"),
        "mkv" => ("video/x-matroska", "video"),
        "ogv" => ("video/ogg", "video"),
        "mp3" => ("audio/mpeg", "audio"),
        "flac" => ("audio/flac", "audio"),
        "wav" => ("audio/wav", "audio"),
        "m4a" => ("audio/mp4", "audio"),
        "ogg" | "opus" => ("audio/ogg", "audio"),
        "png" => ("image/png", "image"),
        "jpg" | "jpeg" => ("image/jpeg", "image"),
        "gif" => ("image/gif", "image"),
        "webp" => ("image/webp", "image"),
        "svg" => ("image/svg+xml", "other"), // svg 可携脚本，只当附件下载
        "pdf" => ("application/pdf", "other"),
        "txt" | "md" | "log" => ("text/plain; charset=utf-8", "text"),
        "json" => ("application/json", "text"),
        "zip" => ("application/zip", "other"),
        "gz" | "tgz" => ("application/gzip", "other"),
        "tar" => ("application/x-tar", "other"),
        "7z" => ("application/x-7z-compressed", "other"),
        _ => ("application/octet-stream", "other"),
    }
}

#[cfg(unix)]
fn available_disk_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let cstr = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(cstr.as_ptr(), &mut stats) } == 0)
        .then(|| stats.f_bavail as u64 * stats.f_frsize as u64)
}

#[cfg(not(unix))]
fn available_disk_bytes(_path: &Path) -> Option<u64> {
    None
}

fn mtime_unix(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

impl StateStore {
    /// 登记一次分享。`snapshot=false`（默认）零复制，只记路径 + 指纹；
    /// `snapshot=true` 复制进托管区，链接不再受原文件删改影响。
    /// `max_bytes` 为 0 表示不限制单文件大小。
    pub fn share_file(
        &self,
        path: &Path,
        title: &str,
        snapshot: bool,
        max_bytes: u64,
    ) -> Result<SharedFile> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolving shared file path: {}", path.display()))?;
        let metadata = std::fs::metadata(&canonical)
            .with_context(|| format!("reading shared file metadata: {}", canonical.display()))?;
        if !metadata.is_file() {
            bail!("shared path is not a regular file: {}", canonical.display());
        }
        if metadata.len() == 0 {
            bail!("shared file is empty: {}", canonical.display());
        }
        if max_bytes > 0 && metadata.len() > max_bytes {
            bail!(
                "shared file is {} bytes which exceeds the configured max_shared_file_bytes limit of {}",
                metadata.len(),
                max_bytes
            );
        }
        let file_name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.contains('\0'))
            .context("shared file needs a valid UTF-8 file name")?
            .to_string();
        let (mime, kind) = shared_file_mime_kind(&canonical);
        let created_at = chrono::Utc::now().to_rfc3339();
        let share_id = format!(
            "share_{}",
            &blake3::hash(
                format!(
                    "{}\0{}\0{}\0{}",
                    canonical.display(),
                    metadata.len(),
                    mtime_unix(&metadata),
                    created_at
                )
                .as_bytes()
            )
            .to_hex()[..24]
        );
        let stored_path = if snapshot {
            let dir = self.shared_files_dir.join(&share_id);
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating snapshot dir: {}", dir.display()))?;
            if let Some(available) = available_disk_bytes(&self.shared_files_dir) {
                if available < metadata.len().saturating_add(SNAPSHOT_FREE_MARGIN_BYTES) {
                    let _ = std::fs::remove_dir(&dir);
                    bail!(
                        "not enough disk space for a snapshot copy: the file needs {} bytes plus a 1 GiB margin but only {} bytes are available. Share without snapshot, or free up space.",
                        metadata.len(),
                        available
                    );
                }
            }
            let target = dir.join(&file_name);
            if let Err(error) = std::fs::copy(&canonical, &target) {
                let _ = std::fs::remove_file(&target);
                let _ = std::fs::remove_dir(&dir);
                return Err(error).with_context(|| {
                    format!("copying shared file snapshot to {}", target.display())
                });
            }
            target
        } else {
            canonical.clone()
        };
        // 指纹取 stored_path（快照即副本自身），下载校验对的也是它。
        let stored_metadata = std::fs::metadata(&stored_path)
            .with_context(|| format!("reading stored file metadata: {}", stored_path.display()))?;
        let record = SharedFile {
            share_id,
            file_name,
            title: title.trim().to_string(),
            mode: if snapshot { "snapshot" } else { "reference" }.to_string(),
            source_path: canonical.display().to_string(),
            stored_path: stored_path.display().to_string(),
            size_bytes: stored_metadata.len(),
            mtime_unix: mtime_unix(&stored_metadata),
            mime: mime.to_string(),
            kind: kind.to_string(),
            created_at,
        };
        if let Err(error) = self.conv_db.insert_shared_file(&record) {
            if snapshot {
                let _ = std::fs::remove_file(&stored_path);
                if let Some(parent) = stored_path.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
            }
            return Err(error);
        }
        Ok(record)
    }

    /// WebUI 上传落地区:先流到这里,登记成功再挪进 `{share_id}/`。
    /// 与托管区同一文件系统,最后一步是 rename 不是复制。
    pub fn shared_files_incoming_dir(&self) -> PathBuf {
        self.shared_files_dir.join(".incoming")
    }

    /// 把一份已经写进落地区的上传文件登记为 snapshot 分享。
    /// `file_name` 已消毒;`staged` 必须在 `shared_files_incoming_dir()` 下。
    pub fn share_uploaded_file(
        &self,
        staged: &Path,
        file_name: &str,
        title: &str,
    ) -> Result<SharedFile> {
        let metadata = std::fs::metadata(staged)
            .with_context(|| format!("reading uploaded file metadata: {}", staged.display()))?;
        if metadata.len() == 0 {
            bail!("uploaded file is empty");
        }
        let (mime, kind) = shared_file_mime_kind(Path::new(file_name));
        let created_at = chrono::Utc::now().to_rfc3339();
        let share_id = format!(
            "share_{}",
            &blake3::hash(
                format!(
                    "upload\0{}\0{}\0{}\0{}",
                    file_name,
                    metadata.len(),
                    mtime_unix(&metadata),
                    created_at
                )
                .as_bytes()
            )
            .to_hex()[..24]
        );
        let dir = self.shared_files_dir.join(&share_id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating share dir: {}", dir.display()))?;
        let stored_path = dir.join(file_name);
        std::fs::rename(staged, &stored_path)
            .with_context(|| format!("moving upload into {}", stored_path.display()))?;
        let record = SharedFile {
            share_id,
            file_name: file_name.to_string(),
            title: title.trim().to_string(),
            mode: "snapshot".to_string(),
            source_path: stored_path.display().to_string(),
            stored_path: stored_path.display().to_string(),
            size_bytes: metadata.len(),
            mtime_unix: mtime_unix(&metadata),
            mime: mime.to_string(),
            kind: kind.to_string(),
            created_at,
        };
        if let Err(error) = self.conv_db.insert_shared_file(&record) {
            let _ = std::fs::remove_file(&stored_path);
            let _ = std::fs::remove_dir(&dir);
            return Err(error);
        }
        Ok(record)
    }

    pub fn list_shared_files(&self) -> Result<Vec<SharedFile>> {
        self.conv_db.list_shared_files()
    }

    pub fn load_shared_file(&self, share_id: &str) -> Result<Option<SharedFile>> {
        self.conv_db.load_shared_file(share_id)
    }

    /// 删除分享条目；快照模式连托管副本一起删。
    pub fn delete_shared_file(&self, share_id: &str) -> Result<bool> {
        let record = self.conv_db.load_shared_file(share_id)?;
        let deleted = self.conv_db.delete_shared_file(share_id)?;
        if let Some(record) = record.filter(|record| record.mode == "snapshot") {
            let stored = PathBuf::from(&record.stored_path);
            // 只清托管区内的路径,防御性检查避免误删任意文件。
            if stored.starts_with(&self.shared_files_dir) {
                let _ = std::fs::remove_file(&stored);
                if let Some(parent) = stored.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
            }
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store(temp: &tempfile::TempDir) -> StateStore {
        StateStore::new(&crate::paths::MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        })
        .unwrap()
    }

    #[test]
    fn uploaded_share_moves_into_managed_storage_as_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = test_store(&temp);
        let incoming = store.shared_files_incoming_dir().join("u1");
        std::fs::create_dir_all(&incoming).unwrap();
        let staged = incoming.join("clip.mp4");
        std::fs::write(&staged, b"vid").unwrap();

        let record = store
            .share_uploaded_file(&staged, "clip.mp4", "剪辑素材")
            .unwrap();
        assert_eq!(record.mode, "snapshot");
        assert_eq!(record.kind, "video");
        assert_eq!(record.title, "剪辑素材");
        assert!(!staged.exists());
        let stored = PathBuf::from(&record.stored_path);
        assert!(stored.starts_with(temp.path().join("data/shared")));
        assert_eq!(std::fs::read(&stored).unwrap(), b"vid");
        assert_eq!(store.list_shared_files().unwrap().len(), 1);

        assert!(store.delete_shared_file(&record.share_id).unwrap());
        assert!(!stored.exists());
    }

    #[test]
    fn reference_share_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let store = test_store(&temp);
        let file = temp.path().join("notes.txt");
        std::fs::write(&file, b"hello lan").unwrap();

        let record = store.share_file(&file, "笔记", false, 0).unwrap();
        assert_eq!(record.mode, "reference");
        assert_eq!(record.file_name, "notes.txt");
        assert_eq!(record.size_bytes, 9);
        assert!(record.share_id.starts_with("share_"));
        // reference 模式零复制:stored_path 就是原文件。
        assert_eq!(
            PathBuf::from(&record.stored_path),
            file.canonicalize().unwrap()
        );

        let listed = store.list_shared_files().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            store.load_shared_file(&record.share_id).unwrap().unwrap(),
            listed[0]
        );

        assert!(store.delete_shared_file(&record.share_id).unwrap());
        assert!(store.list_shared_files().unwrap().is_empty());
        assert!(
            file.exists(),
            "deleting a reference share must not touch the original"
        );
    }

    #[test]
    fn snapshot_share_copies_and_deletes_with_entry() {
        let temp = tempfile::tempdir().unwrap();
        let store = test_store(&temp);
        let file = temp.path().join("clip.mp4");
        std::fs::write(&file, vec![7u8; 2048]).unwrap();

        let record = store.share_file(&file, "", true, 0).unwrap();
        assert_eq!(record.mode, "snapshot");
        assert_eq!(record.kind, "video");
        let stored = PathBuf::from(&record.stored_path);
        assert_ne!(stored, file.canonicalize().unwrap());
        assert!(stored.exists());
        // 原文件删掉,快照分享仍然有效。
        std::fs::remove_file(&file).unwrap();
        assert!(stored.exists());

        assert!(store.delete_shared_file(&record.share_id).unwrap());
        assert!(
            !stored.exists(),
            "snapshot copy must be removed with the entry"
        );
    }

    #[test]
    fn share_rejects_oversized_and_missing_files() {
        let temp = tempfile::tempdir().unwrap();
        let store = test_store(&temp);
        let file = temp.path().join("big.bin");
        std::fs::write(&file, vec![0u8; 1024]).unwrap();
        let error = store.share_file(&file, "", false, 512).unwrap_err();
        assert!(error.to_string().contains("max_shared_file_bytes"));
        assert!(store
            .share_file(&temp.path().join("missing.bin"), "", false, 0)
            .is_err());
    }

    #[test]
    fn maps_extensions_to_mime_and_kind() {
        assert_eq!(
            shared_file_mime_kind(Path::new("a.mp4")),
            ("video/mp4", "video")
        );
        assert_eq!(
            shared_file_mime_kind(Path::new("a.FLAC")),
            ("audio/flac", "audio")
        );
        assert_eq!(
            shared_file_mime_kind(Path::new("a.svg")).1,
            "other",
            "svg must never preview inline"
        );
        assert_eq!(
            shared_file_mime_kind(Path::new("no_extension")),
            ("application/octet-stream", "other")
        );
    }
}
