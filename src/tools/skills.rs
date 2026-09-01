use super::{ToolRegistry, ToolSpec};
use crate::config::AppConfig;
use crate::paths::MiyuPaths;
use crate::skills::{self, SkillEntry, SkillScope};
use anyhow::{Context, Result};
use serde_json::{json, Value};

pub fn register_skills(
    registry: &mut ToolRegistry,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<()> {
    let (entries, fingerprint) = stable_catalog(config, paths)?;
    register_load_skill(registry, config.clone(), paths.clone(), &entries);
    registry.set_skill_catalog_fingerprint(fingerprint);
    Ok(())
}

pub fn refresh_skills(
    registry: &mut ToolRegistry,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<bool> {
    if !registry.contains("load_skill") {
        return Ok(false);
    }
    let Some(snapshot) =
        prepare_skill_refresh(registry.skill_catalog_fingerprint(), config, paths)?
    else {
        return Ok(false);
    };
    apply_skill_refresh(registry, config, paths, snapshot);
    Ok(true)
}

pub(crate) struct SkillCatalogSnapshot {
    entries: Vec<SkillEntry>,
    fingerprint: [u8; 32],
}

pub(crate) fn prepare_skill_refresh(
    current: Option<[u8; 32]>,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<Option<SkillCatalogSnapshot>> {
    let fingerprint = skills::catalog_fingerprint(config, paths)?;
    if current == Some(fingerprint) {
        return Ok(None);
    }
    let (entries, fingerprint) = stable_catalog(config, paths)?;
    Ok(Some(SkillCatalogSnapshot {
        entries,
        fingerprint,
    }))
}

pub(crate) fn apply_skill_refresh(
    registry: &mut ToolRegistry,
    config: &AppConfig,
    paths: &MiyuPaths,
    snapshot: SkillCatalogSnapshot,
) {
    register_load_skill(registry, config.clone(), paths.clone(), &snapshot.entries);
    registry.set_skill_catalog_fingerprint(snapshot.fingerprint);
}

fn stable_catalog(config: &AppConfig, paths: &MiyuPaths) -> Result<(Vec<SkillEntry>, [u8; 32])> {
    for _ in 0..3 {
        let before = skills::catalog_fingerprint(config, paths)?;
        let entries = skills::discover(config, paths)?;
        let after = skills::catalog_fingerprint(config, paths)?;
        if before == after {
            return Ok((entries, after));
        }
    }
    anyhow::bail!("skill catalog kept changing while it was being refreshed")
}

/// 五件 Skill 创作工具合并成 `manage_skill`(08-17):create/update/delete/
/// publish/list_drafts 是同一条创作流水线上的五个动作。
pub fn register_authoring(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    registry.register(
        ToolSpec::new(
            "manage_skill",
            "Author Miyu skills. action=create opens a hidden draft for a new skill; action=update copies an existing skill into an isolated draft; edit only the returned draft with apply_patch, then action=publish validates and atomically publishes it (create drafts never overwrite; update drafts fail if the live skill changed meanwhile). action=delete permanently removes a user skill; action=list_drafts lists retained drafts (drafts untouched for 30 days are pruned first). Scripts inside a skill stay resources and are never registered as tools.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "update", "delete", "publish", "list_drafts"],
                        "description": "Which authoring step to run."
                    },
                    "name": {
                        "type": "string",
                        "pattern": "^[a-z0-9]+(-[a-z0-9]+)*$",
                        "description": "Skill name, required for create/update/delete. Must follow the Agent Skills naming rules."
                    },
                    "description": {
                        "type": "string",
                        "description": "action=create: what the skill does and when it should be used."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["global", "persona"],
                        "description": "global is available to every persona; persona belongs to the current persona. Required for update/delete; for create it defaults to persona (choose global only to share the skill with every persona)."
                    },
                    "draft_id": {
                        "type": "string",
                        "description": "action=publish: draft ID returned by create or update."
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            move |args| {
                let config = config.clone();
                let paths = paths.clone();
                async move {
                    tokio::task::spawn_blocking(move || {
                        match args.get("action").and_then(Value::as_str).unwrap_or_default() {
                            "create" => create_skill(args, &config, &paths),
                            "update" => update_skill(args, &config, &paths),
                            "delete" => delete_skill(args, &config, &paths),
                            "publish" => publish_skill(args, &paths),
                            "list_drafts" => Ok(serde_json::to_string_pretty(&json!({
                                "ok": true,
                                "drafts": skills::list_drafts(&paths)?,
                            }))?),
                            other => anyhow::bail!(
                                "unknown action: {other}; expected create, update, delete, publish or list_drafts"
                            ),
                        }
                    })
                    .await
                    .context("skill authoring worker stopped")?
                }
            },
        )
        .writes(),
    );
}

fn register_load_skill(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    entries: &[SkillEntry],
) {
    // 第一行必须自洽:stub 模式只保留它,原来那句"必须匹配下方列出的可用
    // 技能之一"指向一份 stub 里根本不存在的清单(清单在完整契约里,要经
    // load_tools 取)。清单相关的话统一放进第二段,它只随完整契约出现。
    let description = format!(
        "{}\n\n{}\n\n{}",
        "Load a specialized skill's full instructions and resources into the conversation.",
        "The skill name must match one of the available skills listed below. Use this tool before applying a skill or using any scripts/resources from that skill. Skill allowed-tools metadata never grants Miyu permissions.",
        available_skills_xml(entries),
    );
    registry.register(ToolSpec::new(
        "load_skill",
        description,
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The exact skill name from the available skills list."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        move |args| {
            let config = config.clone();
            let paths = paths.clone();
            async move {
                tokio::task::spawn_blocking(move || load_skill(args, &config, &paths))
                    .await
                    .context("skill loader worker stopped")?
            }
        },
    ));
}

fn load_skill(args: Value, config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let name = required_string(&args, "name")?;
    let loaded = skills::load(&name, config, paths)?;
    let base_dir = loaded
        .base_dir
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "built-in".to_string());
    let files = if loaded.files.is_empty() {
        String::new()
    } else {
        format!(
            "\n<skill_files>\n{}\n</skill_files>",
            loaded
                .files
                .iter()
                .map(|path| format!("  <file>{}</file>", xml_escape(&path.display().to_string())))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let metadata = skill_metadata_xml(&loaded.metadata);
    Ok(format!(
        "<skill_content name=\"{}\" source=\"{}\">\n{}\n<skill_instructions format=\"markdown\">\n{}\n</skill_instructions>\n\n<skill_base_dir>{}</skill_base_dir>{}\n</skill_content>",
        xml_escape(&loaded.metadata.name),
        loaded.source.as_str(),
        metadata,
        xml_escape(&loaded.body),
        xml_escape(&base_dir),
        files,
    ))
}

fn skill_metadata_xml(metadata: &crate::skills::SkillMetadata) -> String {
    let mut fields = vec![format!(
        "  <description>{}</description>",
        xml_escape(&metadata.description)
    )];
    if let Some(license) = &metadata.license {
        fields.push(format!("  <license>{}</license>", xml_escape(license)));
    }
    if let Some(compatibility) = &metadata.compatibility {
        fields.push(format!(
            "  <compatibility>{}</compatibility>",
            xml_escape(compatibility)
        ));
    }
    if let Some(allowed_tools) = &metadata.allowed_tools {
        fields.push(format!(
            "  <allowed_tools grants_permissions=\"false\">{}</allowed_tools>",
            xml_escape(allowed_tools)
        ));
    }
    for (key, value) in &metadata.metadata {
        fields.push(format!(
            "  <entry key=\"{}\">{}</entry>",
            xml_escape(key),
            xml_escape(value)
        ));
    }
    format!("<skill_metadata>\n{}\n</skill_metadata>", fields.join("\n"))
}

fn create_skill(args: Value, config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let name = required_string(&args, "name")?;
    let description = required_string(&args, "description")?;
    // 创建默认落当前人格,不再默认 global(09-01)。在某人格对话里学会/创作的
    // 技能默认属于那个人格,漏给所有人格要显式选 global——尤其自动学习的技能,
    // 默认 global 会让 QQ 线学的习惯出现在别人的自定义人格里。
    let scope = match args.get("scope").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => SkillScope::parse(Some(value))?,
        _ => SkillScope::Persona,
    };
    let draft = skills::create_draft(config, paths, &name, &description, scope)?;
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "state": "draft",
        "draft": draft,
        "next": "Edit only the returned draft with apply_patch, then call publish_skill with draft_id."
    }))?)
}

fn update_skill(args: Value, config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let name = required_string(&args, "name")?;
    let scope_value = required_string(&args, "scope")?;
    let scope = SkillScope::parse(Some(&scope_value))?;
    let draft = skills::update_draft(config, paths, &name, scope)?;
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "state": "draft",
        "draft": draft,
        "next": "Edit only the returned draft with apply_patch, then call publish_skill with draft_id."
    }))?)
}

fn publish_skill(args: Value, paths: &MiyuPaths) -> Result<String> {
    let draft_id = required_string(&args, "draft_id")?;
    let published = skills::publish_draft(paths, &draft_id)?;
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "state": "published",
        "skill": published,
        "catalog_refresh": "next tool round",
    }))?)
}

fn delete_skill(args: Value, config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let name = required_string(&args, "name")?;
    let scope_value = required_string(&args, "scope")?;
    let scope = SkillScope::parse(Some(&scope_value))?;
    let deleted = skills::delete_skill(config, paths, &name, scope)?;
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "state": "deleted",
        "skill": deleted,
        "catalog_refresh": "next tool round",
    }))?)
}

fn required_string(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() {
        anyhow::bail!("{key} is required");
    }
    Ok(value.to_string())
}

fn available_skills_xml(entries: &[SkillEntry]) -> String {
    let items = entries
        .iter()
        .map(|entry| {
            // 08-21 文风批:条目单行化——五行 XML 壳对每技能是纯结构开销,
            // QQ 会话随 load_skill 描述每请求常驻。
            format!(
                "  <skill name=\"{}\" source=\"{}\">{}</skill>",
                xml_escape(&entry.metadata.name),
                entry.source.as_str(),
                xml_escape(&entry.metadata.description),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("<available_skills>\n{items}\n</available_skills>")
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_paths(root: &std::path::Path) -> MiyuPaths {
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
    fn load_skill_description_includes_builtin_creator() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let mut registry = ToolRegistry::new();
        register_skills(&mut registry, &config, &paths).unwrap();
        let description = &registry.get("load_skill").unwrap().description;
        assert!(description.contains("name=\"skill-creator\""));
        assert!(description.contains("source=\"built_in\""));
    }

    #[test]
    fn loaded_skill_exposes_standard_metadata_without_granting_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let directory = paths.skills_dir.join("sample-skill");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: sample-skill\ndescription: Sample workflow\nlicense: MIT\ncompatibility: Miyu\nallowed-tools: run_command\nmetadata:\n  author: test\n---\n\nBody.",
        )
        .unwrap();

        let loaded = load_skill(
            json!({"name": "sample-skill"}),
            &AppConfig::default(),
            &paths,
        )
        .unwrap();
        assert!(loaded.contains("<license>MIT</license>"));
        assert!(loaded.contains("<compatibility>Miyu</compatibility>"));
        assert!(loaded
            .contains("<allowed_tools grants_permissions=\"false\">run_command</allowed_tools>"));
        assert!(loaded.contains("<entry key=\"author\">test</entry>"));
    }

    #[test]
    fn authoring_tools_have_write_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut registry = ToolRegistry::new();
        register_authoring(&mut registry, AppConfig::default(), paths);
        assert_eq!(
            registry.permission("manage_skill").unwrap(),
            super::super::ToolPermission::Writes
        );
    }

    #[test]
    fn refresh_detects_new_skill_once_without_a_watcher() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let mut registry = ToolRegistry::new();
        register_skills(&mut registry, &config, &paths).unwrap();
        let directory = paths.skills_dir.join("new-skill");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: new-skill\ndescription: Newly added skill\n---\n",
        )
        .unwrap();

        assert!(refresh_skills(&mut registry, &config, &paths).unwrap());
        assert!(registry
            .get("load_skill")
            .unwrap()
            .description
            .contains("name=\"new-skill\""));
        assert!(!refresh_skills(&mut registry, &config, &paths).unwrap());
    }

    /// 技能清单只存在于 `load_skill` 的描述里，所以这个工具必须常驻——它一旦
    /// 懒加载，stub 模式只留描述第一行，清单被整段砍掉，模型**看不到任何技能
    /// 名**，只能瞎猜。实测过一次：模型连试 gaming / linux-game-compat /
    /// linux-gaming 三个名字，真名是 linux-game-compatibility，一个没蒙对，
    /// 一轮烧掉 208k token 去找一个一直都在的内置技能。
    #[test]
    fn load_skill_is_always_loaded_so_its_catalog_is_visible() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let mut registry = ToolRegistry::new();
        register_skills(&mut registry, &config, &paths).unwrap();
        let load_skill = registry.get("load_skill").unwrap();
        assert!(load_skill.always_loaded, "懒加载会把技能清单砍掉");
        // 清单确实在描述里,而且 stub 模式下会原样发出去。
        assert!(
            load_skill.description.contains("<available_skills>"),
            "描述里没有清单,常驻也没意义"
        );
        let stub = registry
            .stub_definitions()
            .into_iter()
            .find(|definition| definition.function.name == "load_skill")
            .expect("load_skill 应当在 stub 目录里");
        assert!(
            stub.function.description.contains("name=\"skill-creator\""),
            "stub 里看不到技能名——模型只能猜"
        );
        assert_eq!(
            load_skill.load_policy,
            super::super::tool_descriptions::LoadPolicy::Summary
        );
        assert_eq!(load_skill.groups, vec!["skills"]);
    }

    #[test]
    fn update_skill_requires_an_explicit_scope_at_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let error = update_skill(
            json!({"name": "sample-skill"}),
            &AppConfig::default(),
            &paths,
        )
        .unwrap_err();
        assert!(error.to_string().contains("scope is required"));
    }

    #[test]
    fn delete_skill_requires_an_explicit_scope_at_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let error = delete_skill(
            json!({"name": "sample-skill"}),
            &AppConfig::default(),
            &paths,
        )
        .unwrap_err();
        assert!(error.to_string().contains("scope is required"));
    }
}
