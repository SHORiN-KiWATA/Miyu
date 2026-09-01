mod legacy_migration;
mod resource_migration;
pub(crate) use legacy_migration::*;
pub(crate) use resource_migration::*;

/// Miyu 自己这个可执行文件的路径，**在它可能被替换之前**记下来。
///
/// 好几处功能靠再执行一遍自己来干活：daemon 是 `miyu __daemon`，长图渲染器是
/// `miyu __render_worker`，闹钟和知识库索引也是。它们原本各自调
/// `std::env::current_exe()`，而那在 Linux 上读的是 `/proc/self/exe`——**一旦
/// 磁盘上的文件被换掉（升级安装包、开发时重新编译），这个符号链接就变成
/// `/path/to/miyu (deleted)`，拿它去 spawn 必然 ENOENT。**
///
/// 后果很隐蔽：长回复不再转图片、直接发成大段文字，只在滚动日志里留一条
/// warning，用户看到的是「这功能怎么不работа了」。
///
/// 所以：第一次调用就把结果缓存下来（daemon 启动时立刻预热，那时文件还在），
/// 并且把 `(deleted)` 后缀剥掉——路径本身通常仍指向新装上的那个二进制。
pub fn miyu_executable() -> Result<PathBuf> {
    static EXECUTABLE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(path) = EXECUTABLE.get() {
        return Ok(path.clone());
    }
    let raw = std::env::current_exe().context("locating the Miyu executable")?;
    let resolved = strip_deleted_suffix(&raw).unwrap_or(raw);
    Ok(EXECUTABLE.get_or_init(|| resolved).clone())
}

/// 进程启动早期预热一次，趁二进制还没被换掉。
pub fn prime_miyu_executable() {
    let _ = miyu_executable();
}

/// `/proc/self/exe` 在文件被替换后会读出 `".../miyu (deleted)"`。
/// 剥掉那个后缀，且只在剥完确实存在时才采信——不然宁可用原样报错，
/// 也好过悄悄跑到一个不相干的路径上。
fn strip_deleted_suffix(path: &Path) -> Option<PathBuf> {
    const SUFFIX: &str = " (deleted)";
    let name = path.file_name()?.to_str()?;
    let stripped = name.strip_suffix(SUFFIX)?;
    let candidate = path.with_file_name(stripped);
    candidate.exists().then_some(candidate)
}

use crate::i18n::text as t;
use anyhow::{bail, Context, Result};
use directories::{BaseDirs, UserDirs};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{symlink, DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct MiyuPaths {
    /// Everything below lives under this root (`~/.miyu`, or `MIYU_HOME`).
    /// Kept as its own field because the model is told where Miyu's files are
    /// and guessing it back from a child directory would silently break the
    /// day the layout changes.
    pub root_dir: PathBuf,
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub skills_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub pictures_dir: PathBuf,
    pub fish_hook_file: PathBuf,
    pub bash_hook_file: PathBuf,
    pub zsh_hook_file: PathBuf,
    pub scripts_dir: PathBuf,
    pub system_scripts_dir: PathBuf,
}

impl MiyuPaths {
    pub fn new() -> Result<Self> {
        let base = BaseDirs::new().context(t(
            "could not determine XDG base directories",
            "无法确定 XDG 基础目录",
        ))?;
        let legacy_config_dir = base.config_dir().join("miyu");
        let legacy_data_dir = base.data_dir().join("miyu");
        let legacy_cache_dir = base.cache_dir().join("miyu");
        let legacy_state_dir = base
            .state_dir()
            .unwrap_or_else(|| base.data_dir())
            .join("miyu");
        let legacy_documents_dir = UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(PathBuf::from))
            .unwrap_or_else(|| base.home_dir().join("Documents"))
            .join("Miyu");
        let legacy_pictures_root = std::env::var_os("XDG_PICTURES_DIR")
            .map(PathBuf::from)
            .or_else(|| UserDirs::new().and_then(|dirs| dirs.picture_dir().map(PathBuf::from)))
            .unwrap_or_else(|| base.home_dir().join("Pictures"));
        let explicit_home = std::env::var_os("MIYU_HOME").map(PathBuf::from);
        let root_dir = explicit_home
            .clone()
            .unwrap_or_else(|| base.home_dir().join(".miyu"));
        let config_dir = root_dir.join("config");
        let data_dir = root_dir.join("data");
        let cache_dir = root_dir.join("cache");
        let state_dir = root_dir.join("state");

        let legacy = LegacyLayout {
            config_dir: legacy_config_dir.clone(),
            data_dir: legacy_data_dir.clone(),
            cache_dir: legacy_cache_dir.clone(),
            state_dir: legacy_state_dir.clone(),
            documents_dir: legacy_documents_dir,
            pictures_dirs: vec![
                legacy_pictures_root.join("miyu"),
                legacy_pictures_root.join("Miyu"),
            ],
        };
        let next = Layout {
            root_dir: root_dir.clone(),
            config_dir: config_dir.clone(),
            data_dir: data_dir.clone(),
            cache_dir: cache_dir.clone(),
            state_dir: state_dir.clone(),
        };

        // A client from a newly installed binary may start while the previous
        // daemon still has the legacy SQLite files open. Keep that client on
        // the legacy layout; daemon version negotiation will stop the old
        // process, and the newly spawned daemon performs the migration.
        let migration_enabled = explicit_home.is_none() && !cfg!(test);
        let marker_exists = layout_marker_exists(&next)?;
        let use_legacy_temporarily = migration_enabled
            && !marker_exists
            && legacy.exists()?
            && legacy_daemon_is_running(&legacy);
        let (config_dir, data_dir, cache_dir, state_dir) = if use_legacy_temporarily {
            (
                legacy_config_dir,
                legacy_data_dir,
                legacy_cache_dir,
                legacy_state_dir,
            )
        } else {
            if migration_enabled {
                migrate_legacy_layout(&legacy, &next)?;
            } else if explicit_home.is_some() {
                ensure_private_dir(&root_dir)?;
            }
            (config_dir, data_dir, cache_dir, state_dir)
        };
        let resource_layout = Layout {
            root_dir: root_dir.clone(),
            config_dir: config_dir.clone(),
            data_dir: data_dir.clone(),
            cache_dir: cache_dir.clone(),
            state_dir: state_dir.clone(),
        };
        let resource_marker_exists = resource_layout_marker_exists(&resource_layout)?;
        let daemon_process = current_process_is_daemon();
        let resource_migration_deferred =
            if use_legacy_temporarily || resource_marker_exists || cfg!(test) {
                false
            } else {
                !try_migrate_resource_layout(&resource_layout, daemon_process)?
            };
        let pictures_dir = if use_legacy_temporarily {
            legacy_pictures_root.join("miyu")
        } else {
            data_dir.join("pictures")
        };
        let fish_hook_file = base.config_dir().join("fish/conf.d/miyu.fish");
        let bash_hook_file = config_dir.join("shell/bash-hook.sh");
        let zsh_hook_file = config_dir.join("shell/zsh-hook.zsh");
        let resource_config_dir = if use_legacy_temporarily || resource_migration_deferred {
            config_dir.clone()
        } else {
            data_dir.clone()
        };
        let scripts_dir = resource_config_dir.join("scripts");
        // 内置脚本目录默认在系统前缀下,`MIYU_SYSTEM_SCRIPTS_DIR` 可覆盖——
        // 打包到非标准前缀、或隔离测试时用得上。
        let system_scripts_dir = std::env::var_os("MIYU_SYSTEM_SCRIPTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/share/miyu/scripts"));

        Ok(Self {
            // The canonical home even inside the transient legacy window: that
            // window only exists while an old daemon still holds the XDG files
            // open, and it closes as soon as the new daemon migrates them here.
            root_dir,
            config_file: config_dir.join("config.jsonc"),
            skills_dir: resource_config_dir.join("skills"),
            config_dir,
            data_dir,
            cache_dir,
            state_dir,
            pictures_dir,
            fish_hook_file,
            bash_hook_file,
            zsh_hook_file,
            scripts_dir,
            system_scripts_dir,
        })
    }

    pub fn create_dirs(&self) -> Result<()> {
        let prompts_dir = self.prompts_dir();
        let identities_dir = self.identities_dir();
        let persona_avatars_dir = self.persona_avatars_dir();
        let skill_drafts_dir = self.skill_drafts_dir();
        for directory in [
            &self.config_dir,
            &self.skills_dir,
            &self.data_dir,
            &self.cache_dir,
            &self.state_dir,
            &self.pictures_dir,
            &self.scripts_dir,
            &prompts_dir,
            &identities_dir,
            &persona_avatars_dir,
            &skill_drafts_dir,
        ] {
            ensure_private_dir(directory)?;
        }
        Ok(())
    }

    /// Returns the root used for Miyu-owned, user-authored resources. During
    /// an upgrade this intentionally remains the old config directory until
    /// the resource migration marker has been committed.
    pub fn resource_dir(&self) -> &Path {
        if self.skills_dir == self.data_dir.join("skills") {
            &self.data_dir
        } else {
            &self.config_dir
        }
    }

    pub fn resources_use_config_dir(&self) -> bool {
        self.resource_dir() == self.config_dir
    }

    pub fn legacy_config_dir(&self) -> Option<PathBuf> {
        let base = BaseDirs::new()?;
        (self.config_dir == base.home_dir().join(".miyu/config"))
            .then(|| base.config_dir().join("miyu"))
    }

    pub fn migrated_resource_path(&self, path: &Path) -> Option<PathBuf> {
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.config_dir).ok().or_else(|| {
                self.legacy_config_dir()
                    .as_deref()
                    .and_then(|legacy| path.strip_prefix(legacy).ok())
            })?
        } else {
            path
        };
        let relative = normalize_resource_relative_path(relative)?;
        let namespace = relative.components().next()?.as_os_str().to_str()?;
        if !matches!(
            namespace,
            "skills" | "scripts" | "prompts" | "identities" | "persona-avatars"
        ) {
            return None;
        }
        Some(self.resource_dir().join(relative))
    }

    pub fn prompts_dir(&self) -> PathBuf {
        self.resource_dir().join("prompts")
    }

    pub fn identities_dir(&self) -> PathBuf {
        self.resource_dir().join("identities")
    }

    pub fn persona_avatars_dir(&self) -> PathBuf {
        self.resource_dir().join("persona-avatars")
    }

    pub fn skill_drafts_dir(&self) -> PathBuf {
        self.state_dir.join("skill-drafts")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.cache_dir.join("logs")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(runtime_dir) => runtime_dir_for(
                Path::new(&runtime_dir),
                std::env::var_os("MIYU_HOME").as_deref().map(Path::new),
            ),
            None => self.state_dir.join("miyu"),
        }
    }

    pub fn ipc_socket(&self) -> PathBuf {
        self.runtime_dir().join("core.sock")
    }

    pub fn ipc_lock(&self) -> PathBuf {
        self.runtime_dir().join("core.lock")
    }

    pub fn daemon_start_lock(&self) -> PathBuf {
        self.runtime_dir().join("starter.lock")
    }

    pub fn daemon_launch_state_file(&self) -> PathBuf {
        self.state_dir.join("daemon-launch.json")
    }

    pub fn managed_web_password_dir(&self) -> PathBuf {
        self.state_dir.join("web-passwords")
    }

    pub fn print(&self) {
        println!(
            "{}: {}",
            t("config directory", "配置目录"),
            self.config_dir.display()
        );
        println!(
            "{}: {}",
            t("config file", "配置文件"),
            self.config_file.display()
        );
        println!(
            "{}: {}",
            t("skills directory", "skills 目录"),
            self.skills_dir.display()
        );
        println!(
            "{}: {}",
            t("skill drafts directory", "skill 草稿目录"),
            self.skill_drafts_dir().display()
        );
        println!(
            "{}: {}",
            t("prompts directory", "prompts 目录"),
            self.prompts_dir().display()
        );
        println!(
            "{}: {}",
            t("identities directory", "identities 目录"),
            self.identities_dir().display()
        );
        println!(
            "{}: {}",
            t("persona avatars directory", "人格头像目录"),
            self.persona_avatars_dir().display()
        );
        println!(
            "{}: {}",
            t("data directory", "数据目录"),
            self.data_dir.display()
        );
        println!(
            "{}: {}",
            t("cache directory", "缓存目录"),
            self.cache_dir.display()
        );
        println!(
            "{}: {}",
            t("state directory", "状态目录"),
            self.state_dir.display()
        );
        println!(
            "{}: {}",
            t("log directory", "日志目录"),
            self.logs_dir().display()
        );
        println!(
            "{}: {}",
            t("pictures directory", "图片目录"),
            self.pictures_dir.display()
        );
        println!(
            "{}: {}",
            t("fish hook file", "fish hook 文件"),
            self.fish_hook_file.display()
        );
        println!(
            "{}: {}",
            t("bash hook file", "bash hook 文件"),
            self.bash_hook_file.display()
        );
        println!(
            "{}: {}",
            t("zsh hook file", "zsh hook 文件"),
            self.zsh_hook_file.display()
        );
        println!(
            "{}: {}",
            t("scripts directory", "scripts 目录"),
            self.scripts_dir.display()
        );
        println!(
            "{}: {}",
            t("system scripts directory", "系统 scripts 目录"),
            self.system_scripts_dir.display()
        );
    }
}

fn runtime_dir_for(runtime_root: &Path, explicit_home: Option<&Path>) -> PathBuf {
    let name = explicit_home.map_or_else(
        || "miyu".to_string(),
        |home| {
            let normalized = normalize_home(home);
            let digest = blake3::hash(normalized.as_os_str().as_encoded_bytes());
            format!("miyu-{}", &digest.to_hex()[..12])
        },
    );
    runtime_root.join(name)
}

fn normalize_home(home: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(home) {
        return canonical;
    }
    let absolute = if home.is_absolute() {
        home.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(home)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests;
