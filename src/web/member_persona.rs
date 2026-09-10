//! 成员的私有人格(09-10 分层架构阶段 8,OOBE):`home/<用户>/personas/<slug>/`。
//!
//! 一个目录就是一个人格:`persona.md`(提示词)、`persona.json`(名字/简介/看板文案)、
//! `persona.toml`(启用集:记忆开关 + 插件白名单)、`avatar.*` / `board.*`(图片),
//! 以及运行时长出来的 `memory/`、`skills/`、`scripts/`。会话表里它的 scope 是
//! `home-<用户>-<slug>`(`AppConfig::private_persona_scope`)。
//!
//! 成员用哪个人格记在 `home/<用户>/settings.json`(`active_persona`);None = 用
//! 管理员发布的共享 Miyu。管理员在 `accounts.member_plugins` 里划成员能勾的插件。

use crate::config::{AppConfig, PersonaManifest};
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const MAX_PROMPT_CHARS: usize = 40_000;
pub(crate) const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct UserSettings {
    /// 当前用的私有人格 slug;None = 共享 Miyu。
    pub(crate) active_persona: Option<String>,
    /// 引导做完了(或跳过了)。
    pub(crate) oobe_done: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PersonaMeta {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) board_title: String,
    pub(crate) board_subtitle: String,
    pub(crate) created_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PrivatePersona {
    pub(crate) username: String,
    pub(crate) slug: String,
    pub(crate) dir: PathBuf,
    pub(crate) meta: PersonaMeta,
    pub(crate) manifest: PersonaManifest,
}

impl PrivatePersona {
    pub(crate) fn scope(&self) -> String {
        AppConfig::private_persona_scope(&self.dir).unwrap_or_default()
    }

    pub(crate) fn avatar_path(&self) -> Option<PathBuf> {
        image_in(&self.dir, "avatar")
    }

    pub(crate) fn board_path(&self) -> Option<PathBuf> {
        image_in(&self.dir, "board")
    }

    pub(crate) fn prompt(&self) -> Result<String> {
        let path = self.dir.join("persona.md");
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    }
}

fn image_in(dir: &Path, stem: &str) -> Option<PathBuf> {
    for ext in ["png", "jpg", "jpeg", "webp", "gif"] {
        let path = dir.join(format!("{stem}.{ext}"));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

pub(crate) fn validate_slug(slug: &str) -> Result<()> {
    let count = slug.chars().count();
    if !(2..=32).contains(&count) {
        bail!("persona id must be 2 to 32 characters");
    }
    if !slug
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("persona id may only contain lowercase letters, digits and '-'");
    }
    Ok(())
}

pub(crate) fn personas_root(paths: &MiyuPaths, username: &str) -> PathBuf {
    paths.user_home_dir(username).join("personas")
}

pub(crate) fn settings_path(paths: &MiyuPaths, username: &str) -> PathBuf {
    paths.user_home_dir(username).join("settings.json")
}

pub(crate) fn load_settings(paths: &MiyuPaths, username: &str) -> UserSettings {
    fs::read_to_string(settings_path(paths, username))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub(crate) fn save_settings(
    paths: &MiyuPaths,
    username: &str,
    settings: &UserSettings,
) -> Result<()> {
    let path = settings_path(paths, username);
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_dir(parent)?;
    }
    fs::write(&path, serde_json::to_vec_pretty(settings)?)
        .with_context(|| format!("writing {}", path.display()))
}

pub(crate) fn load_persona(
    paths: &MiyuPaths,
    username: &str,
    slug: &str,
) -> Result<Option<PrivatePersona>> {
    validate_slug(slug)?;
    let dir = personas_root(paths, username).join(slug);
    if !dir.join("persona.md").is_file() {
        return Ok(None);
    }
    let meta: PersonaMeta = fs::read_to_string(dir.join("persona.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let manifest = fs::read_to_string(dir.join("persona.toml"))
        .ok()
        .and_then(|raw| PersonaManifest::parse(&raw).ok())
        .unwrap_or_default();
    Ok(Some(PrivatePersona {
        username: username.to_string(),
        slug: slug.to_string(),
        dir,
        meta,
        manifest,
    }))
}

pub(crate) fn list_personas(paths: &MiyuPaths, username: &str) -> Result<Vec<PrivatePersona>> {
    let root = personas_root(paths, username);
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let Some(slug) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if validate_slug(&slug).is_err() {
            continue;
        }
        if let Some(persona) = load_persona(paths, username, &slug)? {
            out.push(persona);
        }
    }
    out.sort_by(|a, b| {
        a.meta
            .created_at
            .cmp(&b.meta.created_at)
            .then(a.slug.cmp(&b.slug))
    });
    Ok(out)
}

/// 成员当前用的私有人格(settings 指着、目录还在)。
pub(crate) fn active_persona(paths: &MiyuPaths, username: &str) -> Option<PrivatePersona> {
    let settings = load_settings(paths, username);
    let slug = settings.active_persona?;
    load_persona(paths, username, &slug).ok().flatten()
}

/// 按会话表里的 scope 找回成员的人格目录。
pub(crate) fn persona_for_scope(
    paths: &MiyuPaths,
    username: &str,
    scope: &str,
) -> Option<PrivatePersona> {
    if !scope.starts_with("home-") {
        return None;
    }
    list_personas(paths, username)
        .ok()?
        .into_iter()
        .find(|persona| persona.scope() == scope)
}

pub(crate) struct PersonaDraft<'a> {
    pub(crate) name: &'a str,
    pub(crate) description: &'a str,
    pub(crate) prompt: &'a str,
    pub(crate) board_title: &'a str,
    pub(crate) board_subtitle: &'a str,
    pub(crate) memory: bool,
    pub(crate) plugins: Vec<String>,
}

/// 只留管理员放行的插件;子系统里语音/情绪对成员没意义,一律关。
pub(crate) fn manifest_for_member(
    config: &AppConfig,
    memory: bool,
    plugins: &[String],
) -> PersonaManifest {
    let allowed = config.accounts.allowed_member_plugins();
    let mut manifest = PersonaManifest::all();
    manifest.subsystems.memory = memory;
    manifest.subsystems.voice = false;
    manifest.subsystems.emotion = false;
    manifest.plugins.enabled = Some(
        plugins
            .iter()
            .filter(|id| allowed.iter().any(|item| item == *id))
            .cloned()
            .collect(),
    );
    manifest
}

pub(crate) fn create_or_update_persona(
    config: &AppConfig,
    paths: &MiyuPaths,
    username: &str,
    slug: &str,
    draft: &PersonaDraft<'_>,
) -> Result<PrivatePersona> {
    validate_slug(slug)?;
    let name = draft.name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        bail!("persona name must be 1 to 40 characters");
    }
    let prompt = draft.prompt.trim();
    if prompt.is_empty() {
        bail!("persona prompt must not be empty");
    }
    if prompt.chars().count() > MAX_PROMPT_CHARS {
        bail!("persona prompt is too long");
    }
    let dir = personas_root(paths, username).join(slug);
    crate::paths::ensure_private_dir(&paths.user_home_dir(username))?;
    crate::paths::ensure_private_dir(&personas_root(paths, username))?;
    crate::paths::ensure_private_dir(&dir)?;
    let existing: PersonaMeta = fs::read_to_string(dir.join("persona.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let meta = PersonaMeta {
        name: name.to_string(),
        description: draft.description.trim().chars().take(400).collect(),
        board_title: draft.board_title.trim().chars().take(80).collect(),
        board_subtitle: draft.board_subtitle.trim().chars().take(160).collect(),
        created_at: if existing.created_at.is_empty() {
            chrono::Utc::now().to_rfc3339()
        } else {
            existing.created_at
        },
    };
    fs::write(dir.join("persona.md"), format!("{prompt}\n"))?;
    fs::write(dir.join("persona.json"), serde_json::to_vec_pretty(&meta)?)?;
    let manifest = manifest_for_member(config, draft.memory, &draft.plugins);
    fs::write(dir.join("persona.toml"), manifest.to_toml())?;
    Ok(PrivatePersona {
        username: username.to_string(),
        slug: slug.to_string(),
        dir,
        meta,
        manifest,
    })
}

pub(crate) fn delete_persona(paths: &MiyuPaths, username: &str, slug: &str) -> Result<()> {
    validate_slug(slug)?;
    let dir = personas_root(paths, username).join(slug);
    if !dir.is_dir() {
        bail!("persona {slug:?} does not exist");
    }
    fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    let mut settings = load_settings(paths, username);
    if settings.active_persona.as_deref() == Some(slug) {
        settings.active_persona = None;
        save_settings(paths, username, &settings)?;
    }
    Ok(())
}

/// 存头像/看板图:按内容识别格式,旧的同名先删。
pub(crate) fn store_image(dir: &Path, stem: &str, bytes: &[u8]) -> Result<PathBuf> {
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!("image exceeds {} bytes", MAX_IMAGE_BYTES);
    }
    let format = image::guess_format(bytes).context("not a recognised image")?;
    let ext = match format {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::WebP => "webp",
        image::ImageFormat::Gif => "gif",
        _ => bail!("unsupported image format (png/jpeg/webp/gif)"),
    };
    for old in ["png", "jpg", "jpeg", "webp", "gif"] {
        let _ = fs::remove_file(dir.join(format!("{stem}.{old}")));
    }
    let path = dir.join(format!("{stem}.{ext}"));
    fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub(crate) fn remove_image(dir: &Path, stem: &str) {
    for old in ["png", "jpg", "jpeg", "webp", "gif"] {
        let _ = fs::remove_file(dir.join(format!("{stem}.{old}")));
    }
}

/// 给回合用:把成员的私有人格套进配置副本(提示词、清单、记忆、技能、脚本
/// 全跟着目录走)。
pub(crate) fn apply_to_config(config: &mut AppConfig, persona: &PrivatePersona) {
    config.prompt.private_persona_dir = Some(persona.dir.display().to_string());
    // 非空即可:builtin 技能/脚本只给默认人格,私有人格走自己的目录。
    config.prompt.active_persona = format!("{}.md", persona.slug);
    config.prompt.active_identity.clear();
}
