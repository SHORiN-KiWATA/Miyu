//! 人格相关的路径与提示词组装。
//!
//! 每个人格有自己的提示词、记忆库、技能目录。`active_persona_*` 是「当前人格
//! 的那一份」，`dev_scoped` 处理开发模式——它共用目录但要走另一套提示词。
//!
//! `system_prompt_for` 按受众拼提示词：属主能看到主机环境，群里的人不能。

use crate::config::*;

impl AppConfig {
    /// Dev 模式系统提示词:读 `config/dev-prompt.md`,缺失或清空回退内置
    /// 默认一行(极简原则 + 贴近训练分布的措辞,见 08-15 实验记录)。
    pub fn dev_system_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        let path = paths.config_dir.join(DEV_PROMPT_FILE);
        match std::fs::read_to_string(&path) {
            Ok(content) if !content.trim().is_empty() => Ok(content.trim().to_string()),
            _ => Ok(DEFAULT_DEV_SYSTEM_PROMPT.to_string()),
        }
    }

    pub fn system_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        self.system_prompt_for(paths, PromptAudience::Owner)
    }

    pub fn system_prompt_for(&self, paths: &MiyuPaths, audience: PromptAudience) -> Result<String> {
        let mut prompt = self.base_system_prompt(paths)?;
        if audience.includes_user_identity() {
            let user_identity = self.user_identity_prompt(paths)?;
            if !user_identity.trim().is_empty() {
                prompt.push_str("\n\n<current-user-profile>\n");
                prompt.push_str(
                    "This profile describes the user currently interacting with you.\n\n",
                );
                prompt.push_str(user_identity.trim());
                prompt.push_str("\n</current-user-profile>");
            }
        }
        Ok(prompt)
    }

    pub fn base_system_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        let persona = self.active_persona_prompt(paths)?;
        if persona.trim().is_empty() {
            Ok(default_system_prompt())
        } else {
            Ok(persona)
        }
    }

    pub fn custom_system_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        if let Some(prompt) = self
            .system_prompt
            .as_deref()
            .filter(|prompt| !prompt.trim().is_empty())
        {
            return Ok(prompt.to_string());
        }
        let prompt_file = self.system_prompt_path(paths);
        if prompt_file.exists() {
            return Ok(std::fs::read_to_string(prompt_file)?);
        }
        Ok(String::new())
    }

    pub fn prompts_dir_path(&self, paths: &MiyuPaths) -> PathBuf {
        migrated_resource_path(paths, &self.prompt.prompts_dir)
            .unwrap_or_else(|| config_relative_path(paths, &self.prompt.prompts_dir))
    }

    pub fn user_identity_path(&self, paths: &MiyuPaths) -> PathBuf {
        if relative_path_equals(&self.prompt.user_identity_file, "user-identity.md") {
            fallback_resource_file(paths, "identities", "user-identity.md")
        } else if let Some(path) = migrated_fallback_file(
            paths,
            &self.prompt.user_identity_file,
            "identities",
            "user-identity.md",
        ) {
            path
        } else if let Some(path) = migrated_resource_path(paths, &self.prompt.user_identity_file) {
            path
        } else {
            config_relative_path(paths, &self.prompt.user_identity_file)
        }
    }

    pub fn identities_dir_path(&self, paths: &MiyuPaths) -> PathBuf {
        migrated_resource_path(paths, &self.prompt.identities_dir)
            .unwrap_or_else(|| config_relative_path(paths, &self.prompt.identities_dir))
    }

    pub fn persona_path(&self, paths: &MiyuPaths, name: &str) -> PathBuf {
        self.prompts_dir_path(paths).join(name)
    }

    pub fn validate_persona_files(&self, paths: &MiyuPaths) -> Result<()> {
        if self
            .prompt
            .active_persona
            .trim()
            .eq_ignore_ascii_case("system-prompt.md")
        {
            bail!("system-prompt.md is reserved and cannot be used as a persona");
        }
        let directory = self.prompts_dir_path(paths);
        if !directory.exists() {
            return Ok(());
        }
        let mut scopes = HashMap::<String, String>::new();
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") {
                continue;
            }
            if name.eq_ignore_ascii_case("system-prompt.md") {
                continue;
            }
            let scope = persona_scope_name(&name);
            if let Some(existing) = scopes.insert(scope.clone(), name.clone()) {
                bail!(
                    "persona names map to the same persistent scope: {existing} and {name} ({scope})"
                );
            }
        }
        Ok(())
    }

    pub fn identity_path(&self, paths: &MiyuPaths, name: &str) -> PathBuf {
        self.identities_dir_path(paths).join(name)
    }

    pub fn persona_memory_data_dir(&self, paths: &MiyuPaths, persona: &str) -> PathBuf {
        paths
            .data_dir
            .join("personas")
            .join(persona_scope_name(persona))
    }

    pub fn persona_memory_state_dir(&self, paths: &MiyuPaths, persona: &str) -> PathBuf {
        paths
            .state_dir
            .join("personas")
            .join(persona_scope_name(persona))
    }

    pub fn persona_skills_dir(&self, paths: &MiyuPaths, persona: &str) -> PathBuf {
        paths
            .skills_dir
            .join("personas")
            .join(persona_scope_name(persona))
    }

    pub fn persona_scripts_dir(&self, paths: &MiyuPaths, persona: &str) -> PathBuf {
        paths
            .scripts_dir
            .join("personas")
            .join(persona_scope_name(persona))
    }

    pub fn active_persona_scripts_dir(&self, paths: &MiyuPaths) -> PathBuf {
        self.persona_scripts_dir(paths, self.prompt.active_persona.trim())
    }

    /// Sanitized scope name of the active persona; also the namespace key for
    /// sessions and per-persona state directories.
    pub fn active_persona_scope(&self) -> String {
        persona_scope_name(self.prompt.active_persona.trim())
    }

    /// Dev 模式的作用域配置:人格指针换成保留人格 "dev",记忆/技能目录
    /// 随之落入独立命名空间。键是常量人格名而非提示词内容——编辑
    /// dev-prompt.md 只改提示词,永远不会切库丢记忆。
    pub fn dev_scoped(&self) -> AppConfig {
        let mut config = self.clone();
        config.prompt.active_persona = crate::state::DEV_PERSONA.to_string();
        config
    }

    pub fn active_persona_memory_data_dir(&self, paths: &MiyuPaths) -> PathBuf {
        self.persona_memory_data_dir(paths, self.prompt.active_persona.trim())
    }

    pub fn active_persona_memory_state_dir(&self, paths: &MiyuPaths) -> PathBuf {
        self.persona_memory_state_dir(paths, self.prompt.active_persona.trim())
    }

    pub fn active_persona_skills_dir(&self, paths: &MiyuPaths) -> PathBuf {
        self.persona_skills_dir(paths, self.prompt.active_persona.trim())
    }

    pub fn active_persona_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        if !self.prompt.active_persona.trim().is_empty() {
            let path = self.persona_path(paths, self.prompt.active_persona.trim());
            if path.exists() {
                return std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()));
            }
        }
        if let Some(prompt) = self
            .system_prompt
            .as_deref()
            .filter(|prompt| !prompt.trim().is_empty())
        {
            return Ok(prompt.to_string());
        }
        let legacy = self.custom_system_prompt(paths)?;
        if legacy.trim().is_empty() {
            Ok(String::new())
        } else {
            Ok(legacy)
        }
    }

    pub fn user_identity_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        if !self.prompt.active_identity.trim().is_empty() {
            let path = self.identity_path(paths, self.prompt.active_identity.trim());
            if path.exists() {
                return std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()));
            }
        }
        let path = self.user_identity_path(paths);
        if path.exists() {
            return std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()));
        }
        Ok(String::new())
    }

    pub fn system_prompt_path(&self, paths: &MiyuPaths) -> PathBuf {
        let value = self
            .system_prompt_file
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("system-prompt.md");
        if relative_path_equals(value, "system-prompt.md") {
            fallback_resource_file(paths, "prompts", "system-prompt.md")
        } else if let Some(path) =
            migrated_fallback_file(paths, value, "prompts", "system-prompt.md")
        {
            path
        } else if let Some(path) = migrated_resource_path(paths, value) {
            path
        } else {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                paths.config_dir.join(path)
            }
        }
    }
}
