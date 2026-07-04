use super::{ToolRegistry, ToolSpec};
use crate::config::AppConfig;
use crate::i18n::text as t;
use crate::paths::MiyuPaths;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;

pub fn skills_prompt(config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for skills_dir in skill_search_dirs(config, paths) {
        if !skills_dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&skills_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.path().join(".disabled").exists() {
                continue;
            }
            let skill_file = entry.path().join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let raw = std::fs::read_to_string(&skill_file)?;
            let name = skill_name(&raw, &entry.file_name().to_string_lossy());
            if !seen.insert(name.clone()) {
                continue;
            }
            let description = frontmatter_value(&raw, "description").unwrap_or_default();
            let body = strip_frontmatter(&raw);
            entries.push(format!(
                "- {name}: {description}\n  {}",
                compact_skill_body(&body)
            ));
        }
    }
    if entries.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "<available-skills>\n这些是已安装的 skills。遇到匹配任务时主动参考。当前不支持创建、保存或自动生成新的 skill；不要把 skill 内容保存到知识库。\n{}\n</available-skills>",
        entries.join("\n")
    ))
}

pub fn register_skills(
    registry: &mut ToolRegistry,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for skills_dir in skill_search_dirs(config, paths) {
        if !skills_dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&skills_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let skill_dir = entry.path();
            if skill_dir.join(".disabled").exists() {
                continue;
            }
            let skill_file = skill_dir.join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let raw = std::fs::read_to_string(&skill_file)?;
            let name = skill_name(&raw, &entry.file_name().to_string_lossy());
            if !seen.insert(name.clone()) {
                continue;
            }
        }
    }
    register_load_skill(registry, config, paths);
    Ok(())
}

fn register_load_skill(registry: &mut ToolRegistry, config: &AppConfig, paths: &MiyuPaths) {
    let skill_dirs = skill_search_dirs(config, paths);
    registry.register(ToolSpec::new(
        "load_skill",
        t(
            "Load a specialized skill's full instructions and resources into the conversation. The skill name must match one of the available skills listed in the system prompt.",
            "加载指定技能的完整指令和资源到当前对话。技能名称必须匹配系统提示中列出的可用技能之一。",
        ),
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": t("The name of the skill from the available skills list.", "可用技能列表中的技能名称。")
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        move |args| {
            let skill_dirs = skill_dirs.clone();
            async move { load_skill(args, &skill_dirs) }
        },
    ));
}

fn load_skill(args: Value, skill_dirs: &[PathBuf]) -> Result<String> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        anyhow::bail!("skill name is required");
    }
    for dir in skill_dirs {
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let skill_dir = entry.path();
            let skill_file = skill_dir.join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let raw = std::fs::read_to_string(&skill_file)?;
            let skill_name = skill_name(&raw, &entry.file_name().to_string_lossy());
            if skill_name != name {
                continue;
            }
            let body = strip_frontmatter(&raw);
            let base_dir = skill_dir.display().to_string();
            let mut files = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&skill_dir) {
                for file_entry in entries.flatten() {
                    let fname = file_entry.file_name().to_string_lossy().to_string();
                    if fname == "SKILL.md" || fname.starts_with('.') {
                        continue;
                    }
                    files.push(file_entry.path().display().to_string());
                }
            }
            files.sort();
            let files_xml = if files.is_empty() {
                String::new()
            } else {
                let items = files
                    .iter()
                    .map(|f| format!("<file>{f}</file>"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("\n<skill_files>\n{items}\n</skill_files>")
            };
            return Ok(format!(
                "<skill_content name=\"{name}\">\n# Skill: {name}\n\n{body}\n\nBase directory for this skill: {base_dir}\nRelative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.\n{files_xml}\n</skill_content>"
            ));
        }
    }
    anyhow::bail!("skill not found: {name}");
}

fn skill_search_dirs(config: &AppConfig, paths: &MiyuPaths) -> Vec<PathBuf> {
    let mut dirs = vec![paths.skills_dir.clone()];
    let active = config.active_persona_skills_dir(paths);
    if active != paths.skills_dir {
        dirs.push(active);
    }
    dirs
}

fn skill_name(raw: &str, fallback: &str) -> String {
    frontmatter_value(raw, "name").unwrap_or_else(|| fallback.to_string())
}

fn frontmatter_value(raw: &str, key: &str) -> Option<String> {
    let mut lines = raw.lines();
    if lines.next()? != "---" {
        return None;
    }
    for line in lines {
        if line == "---" {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim() == key {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn strip_frontmatter(raw: &str) -> String {
    let mut lines = raw.lines();
    if lines.next() != Some("---") {
        return raw.to_string();
    }
    for line in lines.by_ref() {
        if line == "---" {
            return lines.collect::<Vec<_>>().join("\n");
        }
    }
    raw.to_string()
}

fn compact_skill_body(body: &str) -> String {
    let text = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > 700 {
        format!("{}...", text.chars().take(697).collect::<String>())
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(root: &std::path::Path) -> MiyuPaths {
        MiyuPaths {
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            secrets_file: root.join("config/secrets.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish/miyu.fish"),
            bash_hook_file: root.join("shell/bash-hook.sh"),
            zsh_hook_file: root.join("shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        }
    }

    #[test]
    fn skills_prompt_reads_global_skills_dir() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let skill_dir = paths.skills_dir.join("gpu-passthrough");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: gpu-passthrough\ndescription: GPU switching\n---\n\nUse `gpustoggle --status`.",
        )
        .unwrap();
        let config = AppConfig::default();
        let prompt = skills_prompt(&config, &paths).unwrap();
        assert!(prompt.contains("gpu-passthrough"));
        assert!(prompt.contains("GPU switching"));
    }
}
