mod draft;
mod manifest;
pub(crate) use draft::*;
pub(crate) use manifest::*;

use crate::config::{persona_scope_name, AppConfig};
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use yaml_rust2::scanner::{Scanner, Token, TokenType};
use yaml_rust2::{Yaml, YamlLoader};

/// Skills compiled into the binary: (name, raw SKILL.md, platform_wide). A
/// user skill of the same name in the persona/global directories overrides the
/// built-in.
///
/// 内置技能默认属于 Miyu 这个出厂人格,只在默认人格下可见——别人换上自定义
/// 人格拿到的是纯净状态(09-01)。唯一例外是 `platform_wide=true` 的
/// skill-creator:它是"如何扩展自己"的元能力,任何人格(包括从白板捏起的)
/// 想给自己加技能都得用它,归人格等于锁死自定义角色的自我扩展入口。
///
/// 源码布局与脚本对齐:平台级(skill-creator)在 `src/skills/` 顶层,人格级在
/// `src/skills/personas/default/`。技能是 `include_str!` 编译进二进制的,目录
/// 只是组织形式;运行时门控靠 `platform_wide` 标记,不靠目录(与脚本层用
/// 目录做隐式门不同——脚本是磁盘扫描,技能是编译常量)。
const BUILTIN_SKILLS: &[(&str, &str, bool)] = &[
    (
        "skill-creator",
        include_str!("../skills/skill-creator.md"),
        true,
    ),
    (
        "linux-input-method-diagnose",
        include_str!("../skills/personas/default/linux-input-method-diagnose.md"),
        false,
    ),
    (
        "linux-game-compatibility",
        include_str!("../skills/personas/default/linux-game-compatibility.md"),
        false,
    ),
];

/// 内置资源(技能/脚本)默认只属于 Miyu 出厂人格。判据:`active_persona` 去空
/// 白后为空 = scope "default" = Miyu 本人。非默认人格下,非平台级的内置资源
/// 一律隐藏,换上自定义人格即得纯净状态。
pub(crate) fn is_default_persona(config: &AppConfig) -> bool {
    config.prompt.active_persona.trim().is_empty()
}
const MAX_SKILL_CATALOG_ENTRIES: usize = 256;
const MAX_SKILL_ROOT_DIRECTORIES: usize = 1_024;
const MAX_SKILL_RESOURCE_ENTRIES: usize = 256;

pub fn discover(config: &AppConfig, paths: &MiyuPaths) -> Result<Vec<SkillEntry>> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for (root, source) in skill_roots(config, paths) {
        for directory in sorted_skill_directories(&root)? {
            if directory.join(".disabled").exists() {
                continue;
            }
            let skill_file = directory.join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let raw = match read_skill_file(&skill_file) {
                Ok(raw) => raw,
                Err(error) => {
                    tracing::warn!(path = %skill_file.display(), error = %error, "skipping unreadable skill");
                    continue;
                }
            };
            let directory_name = directory
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let metadata = match parse_skill_metadata(&raw, Some(directory_name)) {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::warn!(path = %skill_file.display(), error = %error, "skipping invalid skill");
                    continue;
                }
            };
            if seen.insert(metadata.name.clone()) {
                if entries.len() >= MAX_SKILL_CATALOG_ENTRIES.saturating_sub(1) {
                    bail!("skill catalog exceeds the {MAX_SKILL_CATALOG_ENTRIES} entry limit");
                }
                entries.push(SkillEntry {
                    metadata,
                    source,
                    directory: Some(directory),
                });
            }
        }
    }
    let default_persona = is_default_persona(config);
    for (name, raw, platform_wide) in BUILTIN_SKILLS {
        if !platform_wide && !default_persona {
            continue;
        }
        if !seen.contains(*name) {
            entries.push(SkillEntry {
                metadata: parse_skill_metadata(raw, Some(name))?,
                source: SkillSource::BuiltIn,
                directory: None,
            });
        }
    }
    Ok(entries)
}

pub fn catalog_fingerprint(config: &AppConfig, paths: &MiyuPaths) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    for (root, source) in skill_roots(config, paths) {
        hasher.update(source.as_str().as_bytes());
        hasher.update(root.as_os_str().as_encoded_bytes());
        for directory in sorted_skill_directories(&root)? {
            hasher.update(directory.as_os_str().as_encoded_bytes());
            hash_metadata(&mut hasher, &directory.join(".disabled"))?;
            hash_metadata(&mut hasher, &directory.join("SKILL.md"))?;
        }
    }
    // 指纹要把「本人格能看见哪些内置技能」也算进去:否则切换人格后目录没变、
    // 指纹不变,而可见的内置集合已经变了,催化缓存会拿旧目录充数。
    let default_persona = is_default_persona(config);
    for (name, raw, platform_wide) in BUILTIN_SKILLS {
        if !platform_wide && !default_persona {
            continue;
        }
        hasher.update(name.as_bytes());
        hasher.update(raw.as_bytes());
    }
    Ok(*hasher.finalize().as_bytes())
}

pub fn load(name: &str, config: &AppConfig, paths: &MiyuPaths) -> Result<LoadedSkill> {
    let name = name.trim();
    if name.is_empty() {
        bail!("skill name is required");
    }
    let entry = discover(config, paths)?
        .into_iter()
        .find(|entry| entry.metadata.name == name)
        .ok_or_else(|| anyhow::anyhow!("skill not found: {name}"))?;
    if let Some(directory) = entry.directory {
        let raw = read_skill_file(&directory.join("SKILL.md"))?;
        let (metadata, body) = parse_skill_document(&raw, Some(name))?;
        let mut files = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "SKILL.md" || name.to_string_lossy().starts_with('.') {
                continue;
            }
            if files.len() >= MAX_SKILL_RESOURCE_ENTRIES {
                bail!(
                    "skill resource manifest exceeds the {MAX_SKILL_RESOURCE_ENTRIES} entry limit"
                );
            }
            files.push(entry.path());
        }
        files.sort();
        return Ok(LoadedSkill {
            metadata,
            body,
            source: entry.source,
            base_dir: Some(directory),
            files,
        });
    }
    // 非默认人格看不见非平台级内置技能,自然也加载不了——与 discover 的
    // 可见性保持一致,不然模型照着历史 load 能把隐藏技能捞回来。
    let default_persona = is_default_persona(config);
    let raw = BUILTIN_SKILLS
        .iter()
        .find(|(builtin_name, _, platform_wide)| {
            *builtin_name == name && (*platform_wide || default_persona)
        })
        .map(|(_, raw, _)| *raw)
        .with_context(|| format!("skill not found: {name}"))?;
    let (metadata, body) = parse_skill_document(raw, Some(name))?;
    Ok(LoadedSkill {
        metadata,
        body,
        source: SkillSource::BuiltIn,
        base_dir: None,
        files: Vec::new(),
    })
}

pub fn is_generated_skill(raw: &str) -> bool {
    parse_skill_metadata(raw, None)
        .ok()
        .and_then(|metadata| metadata.metadata.get("miyu.generated").cloned())
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        || raw.contains("generated_by: miyu")
        || raw.contains("Auto-learned method from assistant conversation")
        || raw.contains("Auto-learned method from Miyu conversation")
}

fn skill_roots(config: &AppConfig, paths: &MiyuPaths) -> Vec<(PathBuf, SkillSource)> {
    vec![
        (
            config.active_persona_skills_dir(paths),
            SkillSource::Persona,
        ),
        (paths.skills_dir.clone(), SkillSource::Global),
    ]
}

fn sorted_skill_directories(root: &Path) -> Result<Vec<PathBuf>> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        bail!("skill root must not be a symbolic link: {}", root.display());
    }
    if !metadata.is_dir() {
        bail!("skill root is not a directory: {}", root.display());
    }
    let mut directories = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
            if directories.len() >= MAX_SKILL_ROOT_DIRECTORIES {
                bail!("skill root exceeds the {MAX_SKILL_ROOT_DIRECTORIES} directory-entry limit");
            }
            directories.push(entry.path());
        }
    }
    directories.sort();
    Ok(directories)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(root: &Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("data/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("data/pictures"),
            fish_hook_file: root.join("fish/miyu.fish"),
            bash_hook_file: root.join("config/shell/bash-hook.sh"),
            zsh_hook_file: root.join("config/shell/zsh-hook.zsh"),
            scripts_dir: root.join("data/scripts"),
            system_scripts_dir: PathBuf::new(),
        }
    }

    #[test]
    fn parses_standard_frontmatter_fields() {
        let raw = "---\nname: sample-skill\ndescription: Sample workflow\nlicense: MIT\ncompatibility: Miyu\nallowed-tools: read_file\nmetadata:\n  author: test\n---\n\nBody.";
        let metadata = parse_skill_metadata(raw, Some("sample-skill")).unwrap();
        assert_eq!(metadata.license.as_deref(), Some("MIT"));
        assert_eq!(metadata.compatibility.as_deref(), Some("Miyu"));
        assert_eq!(metadata.allowed_tools.as_deref(), Some("read_file"));
        assert_eq!(
            metadata.metadata.get("author").map(String::as_str),
            Some("test")
        );
    }

    #[test]
    fn rejects_yaml_anchors_before_loading_frontmatter() {
        let raw = "---\nname: sample-skill\ndescription: &description Sample workflow\nmetadata:\n  copied: *description\n---\n";
        let error = parse_skill_metadata(raw, Some("sample-skill")).unwrap_err();
        assert!(error.to_string().contains("anchors or aliases"));
    }

    #[test]
    fn persona_skill_overrides_global_and_builtin() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let global = paths.skills_dir.join(BUILTIN_SKILLS[0].0);
        let persona = config
            .active_persona_skills_dir(&paths)
            .join(BUILTIN_SKILLS[0].0);
        for (directory, description) in [(&global, "global"), (&persona, "persona")] {
            fs::create_dir_all(directory).unwrap();
            fs::write(
                directory.join("SKILL.md"),
                format!(
                    "---\nname: {}\ndescription: {description}\n---\n",
                    BUILTIN_SKILLS[0].0
                ),
            )
            .unwrap();
        }
        let entries = discover(&config, &paths).unwrap();
        let creator = entries
            .iter()
            .find(|entry| entry.metadata.name == BUILTIN_SKILLS[0].0)
            .unwrap();
        assert_eq!(creator.source, SkillSource::Persona);
        assert_eq!(creator.metadata.description, "persona");
    }

    /// 内置技能默认属于 Miyu 出厂人格:默认人格看得见非平台级内置技能,
    /// 自定义人格只剩平台级(skill-creator)。
    #[test]
    fn builtin_skills_are_persona_gated_except_platform_wide() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());

        let default_config = AppConfig::default();
        let names: BTreeSet<String> = discover(&default_config, &paths)
            .unwrap()
            .into_iter()
            .filter(|entry| entry.source == SkillSource::BuiltIn)
            .map(|entry| entry.metadata.name)
            .collect();
        assert!(names.contains("skill-creator"));
        assert!(names.contains("linux-game-compatibility"));

        let mut custom = AppConfig::default();
        custom.prompt.active_persona = "alter".to_string();
        let custom_names: BTreeSet<String> = discover(&custom, &paths)
            .unwrap()
            .into_iter()
            .filter(|entry| entry.source == SkillSource::BuiltIn)
            .map(|entry| entry.metadata.name)
            .collect();
        assert_eq!(
            custom_names,
            BTreeSet::from(["skill-creator".to_string()]),
            "自定义人格只应看到平台级内置技能"
        );

        // 隐藏的内置技能连 load 都拒绝,不让模型照历史捞回。
        assert!(load("linux-game-compatibility", &custom, &paths).is_err());
        assert!(load("skill-creator", &custom, &paths).is_ok());

        // 指纹随可见集合变化:换人格后目录没动,指纹也必须不同。
        assert_ne!(
            catalog_fingerprint(&default_config, &paths).unwrap(),
            catalog_fingerprint(&custom, &paths).unwrap(),
        );
    }

    #[test]
    fn create_and_publish_draft_never_overwrites() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let draft = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let published = publish_draft(&paths, &draft.id).unwrap();
        assert!(Path::new(&published.path).join("SKILL.md").is_file());
        assert!(create_draft(
            &config,
            &paths,
            "sample-skill",
            "Duplicate",
            SkillScope::Global,
        )
        .is_err());
    }

    #[test]
    fn deletes_global_and_current_persona_skills() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        for scope in [SkillScope::Global, SkillScope::Persona] {
            let draft = create_draft(
                &config,
                &paths,
                "sample-skill",
                "Use for sample tasks",
                scope,
            )
            .unwrap();
            publish_draft(&paths, &draft.id).unwrap();
        }

        let global = delete_skill(&config, &paths, "sample-skill", SkillScope::Global).unwrap();
        assert_eq!(global.scope, "global");
        assert!(!paths.skills_dir.join("sample-skill").exists());
        assert!(config
            .active_persona_skills_dir(&paths)
            .join("sample-skill")
            .is_dir());

        let persona = delete_skill(&config, &paths, "sample-skill", SkillScope::Persona).unwrap();
        assert_eq!(persona.scope, "persona");
        assert!(!config
            .active_persona_skills_dir(&paths)
            .join("sample-skill")
            .exists());
        assert!(delete_skill(&config, &paths, "sample-skill", SkillScope::Global).is_err());
    }

    #[test]
    fn update_draft_detects_concurrent_edits() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let created = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        publish_draft(&paths, &created.id).unwrap();
        let update = update_draft(&config, &paths, "sample-skill", SkillScope::Global).unwrap();
        fs::write(
            paths.skills_dir.join("sample-skill/SKILL.md"),
            "---\nname: sample-skill\ndescription: Changed elsewhere\n---\n",
        )
        .unwrap();
        assert!(publish_draft(&paths, &update.id).is_err());
    }

    #[test]
    fn two_update_drafts_from_the_same_revision_cannot_both_publish() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let created = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        publish_draft(&paths, &created.id).unwrap();
        let first = update_draft(&config, &paths, "sample-skill", SkillScope::Global).unwrap();
        let second = update_draft(&config, &paths, "sample-skill", SkillScope::Global).unwrap();
        fs::write(
            &first.skill_file,
            "---\nname: sample-skill\ndescription: First update\n---\n",
        )
        .unwrap();
        fs::write(
            &second.skill_file,
            "---\nname: sample-skill\ndescription: Second update\n---\n",
        )
        .unwrap();

        publish_draft(&paths, &first.id).unwrap();
        assert!(publish_draft(&paths, &second.id).is_err());
        assert!(
            fs::read_to_string(paths.skills_dir.join("sample-skill/SKILL.md"))
                .unwrap()
                .contains("First update")
        );
    }

    #[test]
    fn live_edit_detected_after_exchange_is_atomically_restored() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("sample-skill");
        let staged = temp.path().join(".stage");
        for (directory, description) in [(&target, "Original"), (&staged, "Updated")] {
            fs::create_dir(directory).unwrap();
            fs::write(
                directory.join("SKILL.md"),
                format!("---\nname: sample-skill\ndescription: {description}\n---\n"),
            )
            .unwrap();
        }
        let expected = skill_revision(&target).unwrap();
        fs::write(
            target.join("SKILL.md"),
            "---\nname: sample-skill\ndescription: Manual edit\n---\n",
        )
        .unwrap();

        let mut guard = StagedDirectory::new(staged.clone());
        assert!(install_updated_skill(&staged, &target, &expected, &mut guard).is_err());
        assert!(fs::read_to_string(target.join("SKILL.md"))
            .unwrap()
            .contains("Manual edit"));
        assert!(fs::read_to_string(staged.join("SKILL.md"))
            .unwrap()
            .contains("Updated"));
    }

    #[test]
    fn tampered_persona_scope_cannot_escape_the_skill_root() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let draft = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Persona,
        )
        .unwrap();
        let manifest_path = paths
            .skill_drafts_dir()
            .join(&draft.id)
            .join(DRAFT_MANIFEST);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["persona_scope"] = serde_json::Value::String("../../outside".to_string());
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        assert!(publish_draft(&paths, &draft.id).is_err());
        assert!(!paths.data_dir.join("outside/sample-skill").exists());
    }

    #[cfg(unix)]
    #[test]
    fn publish_rejects_a_symlinked_draft_package() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let draft = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let draft_root = paths.skill_drafts_dir().join(&draft.id);
        let package = draft_root.join(DRAFT_PACKAGE_DIR);
        let outside = temp.path().join("outside-package");
        fs::create_dir_all(outside.join("sample-skill")).unwrap();
        fs::write(
            outside.join("sample-skill/SKILL.md"),
            "---\nname: sample-skill\ndescription: Outside\n---\n",
        )
        .unwrap();
        fs::remove_dir_all(&package).unwrap();
        symlink(&outside, &package).unwrap();

        assert!(publish_draft(&paths, &draft.id).is_err());
        assert!(!paths.skills_dir.join("sample-skill").exists());
    }

    #[test]
    fn expired_draft_cannot_be_published_directly() {
        fn set_modified_recursive(path: &Path, modified: SystemTime) {
            if path.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    set_modified_recursive(&entry.unwrap().path(), modified);
                }
            }
            File::open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let draft = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let draft_root = paths.skill_drafts_dir().join(&draft.id);
        let expired = SystemTime::now() - DRAFT_RETENTION - Duration::from_secs(60);
        set_modified_recursive(&draft_root, expired);

        assert!(publish_draft(&paths, &draft.id).is_err());
        assert!(!draft_root.exists());
        assert!(!paths.skills_dir.join("sample-skill").exists());
    }

    #[test]
    fn malformed_over_limit_draft_is_removed_during_pruning() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let draft = create_draft(
            &AppConfig::default(),
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let mut directory = PathBuf::from(&draft.skill_dir);
        for index in 0..=(MAX_SKILL_PACKAGE_DEPTH + 5) {
            directory.push(format!("level-{index}"));
        }
        fs::create_dir_all(directory).unwrap();

        assert_eq!(prune_expired_drafts(&paths).unwrap(), 1);
        assert!(!paths.skill_drafts_dir().join(&draft.id).exists());
    }

    #[test]
    fn future_draft_timestamps_are_not_treated_as_expired() {
        fn set_modified_recursive(path: &Path, modified: SystemTime) {
            if path.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    set_modified_recursive(&entry.unwrap().path(), modified);
                }
            }
            File::open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let draft = create_draft(
            &AppConfig::default(),
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let draft_root = paths.skill_drafts_dir().join(&draft.id);
        set_modified_recursive(
            &draft_root,
            SystemTime::now() + Duration::from_secs(24 * 60 * 60),
        );

        assert_eq!(prune_expired_drafts(&paths).unwrap(), 0);
        assert!(draft_root.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn revision_tracks_empty_directories_and_executable_bits() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sample-skill");
        fs::create_dir_all(&root).unwrap();
        let skill_file = root.join("SKILL.md");
        fs::write(
            &skill_file,
            "---\nname: sample-skill\ndescription: Sample\n---\n",
        )
        .unwrap();
        let initial = skill_revision(&root).unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        let with_directory = skill_revision(&root).unwrap();
        assert_ne!(initial, with_directory);
        let mut permissions = fs::metadata(&skill_file).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&skill_file, permissions).unwrap();
        assert_ne!(with_directory, skill_revision(&root).unwrap());
    }

    #[test]
    fn publish_rejects_excessive_directory_depth() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let draft = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let mut directory = PathBuf::from(&draft.skill_dir);
        for index in 0..=MAX_SKILL_PACKAGE_DEPTH {
            directory.push(format!("level-{index}"));
        }
        fs::create_dir_all(directory).unwrap();

        assert!(publish_draft(&paths, &draft.id).is_err());
        assert!(!paths.skills_dir.join("sample-skill").exists());
    }
}
