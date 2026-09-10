//! persona 清单(09-10 分层架构阶段 4):一个 persona 目录里的 `persona.toml`,
//! 声明它启用哪些**子系统**(挂进回合流水线多个点的:记忆、技能、人格提醒、
//! 语音、情绪)和哪些**插件**(只往工具面加东西的:内置插件与外装脚本/MCP)。
//!
//! 这是「模式」退场后唯一的配置单位:dev 是启用集为空的内置 persona,默认人格
//! 是「全部启用」。运行时按启用集**决定构造什么**,不是装了再关——记忆关着
//! 就不建库、不注入、不写日记(setup.rs / turn_loop 按这里裁决)。
//!
//! 文件缺失时按内置默认:persona 名为 `dev` → [`PersonaManifest::core_only`],
//! 其余 → [`PersonaManifest::all`]。解析失败记 warn 并退回默认,不让一个手写
//! 错误把 persona 整个弄哑。

use crate::config::AppConfig;
use crate::paths::MiyuPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

pub const PERSONA_MANIFEST_FILE: &str = "persona.toml";

/// 插件 id:与 `tools::compose_registry` 里的注册单元一一对应。写在这里是为了
/// 让 persona.toml 里的名字有一份真相源,拼错的名字在 [`PersonaManifest::validate`]
/// 里能被指出来。
pub const PLUGIN_IDS: &[&str] = &[
    "files",
    "usage_query",
    "alarm",
    "exchange_rate",
    "archlinux",
    "api_quota",
    "print_image",
    "memes",
    "platform_outreach",
    "web_images",
    "deep_research",
    "image_generation",
    "knowledge_base",
    "package_advisor",
    "diagnostics",
    "ledger",
    "scripts",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Subsystems {
    /// 长期记忆:工具、每轮联想注入、回合后日记/经历、逐出库、系统提示前言。
    pub memory: bool,
    /// 技能目录扫描与 load_skill / manage_skill。
    pub skills: bool,
    /// 人格提醒(化石注入,间隔仍在 config.prompt.persona_reminder_interval)。
    pub persona_reminder: bool,
    /// 语音:唤醒对话、听写、TTS 工具(speak / voice_chat)。
    pub voice: bool,
    /// 情绪与好感度(只在通讯平台层生效;这里是 persona 的意愿位)。
    pub emotion: bool,
}

impl Default for Subsystems {
    fn default() -> Self {
        Self {
            memory: true,
            skills: true,
            persona_reminder: true,
            voice: true,
            emotion: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSelection {
    /// 缺省(None)= 本机装了的、config 开着的全部;写了就是白名单。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersonaManifest {
    pub subsystems: Subsystems,
    pub plugins: PluginSelection,
}

impl Default for PersonaManifest {
    fn default() -> Self {
        Self::all()
    }
}

impl PersonaManifest {
    /// 默认人格:子系统全开,插件按 config 全上——今天的 normal。
    pub fn all() -> Self {
        Self {
            subsystems: Subsystems::default(),
            plugins: PluginSelection::default(),
        }
    }

    /// dev:core 之上一件都不挂。唯一例外是 `platform_outreach`(写代码时
    /// 「跑完发我手机」是真需求,09-05 用户拍板),它自己还受「终端外发开着且
    /// QQ 连着」的机器条件。
    pub fn core_only() -> Self {
        Self {
            subsystems: Subsystems {
                memory: false,
                skills: false,
                persona_reminder: false,
                voice: false,
                emotion: false,
            },
            plugins: PluginSelection {
                enabled: Some(vec!["platform_outreach".to_string()]),
            },
        }
    }

    pub fn builtin_for(persona: &str) -> Self {
        if persona.trim() == crate::state::DEV_PERSONA {
            Self::core_only()
        } else {
            Self::all()
        }
    }

    pub fn manifest_path(config: &AppConfig, paths: &MiyuPaths, persona: &str) -> PathBuf {
        config
            .persona_memory_data_dir(paths, persona)
            .join(PERSONA_MANIFEST_FILE)
    }

    /// 读 persona 目录里的清单;没有文件用内置默认,坏文件记 warn 退回默认。
    pub fn load(config: &AppConfig, paths: &MiyuPaths, persona: &str) -> Self {
        let path = Self::manifest_path(config, paths, persona);
        match std::fs::read_to_string(&path) {
            Ok(raw) => match Self::parse(&raw) {
                Ok(manifest) => manifest,
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "persona.toml did not parse; using the built-in defaults"
                    );
                    Self::builtin_for(persona)
                }
            },
            Err(_) => Self::builtin_for(persona),
        }
    }

    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let manifest: Self = toml::from_str(raw)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// 白名单里出现不认识的插件 id 是错误——静默忽略会让用户以为开了。
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(enabled) = &self.plugins.enabled {
            let known: BTreeSet<&str> = PLUGIN_IDS.iter().copied().collect();
            let unknown: Vec<&str> = enabled
                .iter()
                .map(String::as_str)
                .filter(|id| !known.contains(id))
                .collect();
            if !unknown.is_empty() {
                anyhow::bail!(
                    "unknown plugin id(s) in persona.toml: {}; known: {}",
                    unknown.join(", "),
                    PLUGIN_IDS.join(", ")
                );
            }
        }
        Ok(())
    }

    pub fn plugin_enabled(&self, id: &str) -> bool {
        match &self.plugins.enabled {
            None => true,
            Some(list) => list.iter().any(|item| item == id),
        }
    }

    /// 这个 persona 的记忆子系统是否构造:persona 意愿 × 机器配置。
    pub fn memory_enabled(&self, config: &AppConfig) -> bool {
        self.subsystems.memory && config.memory_config().enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_all_on_and_dev_is_core_only() {
        let all = PersonaManifest::all();
        assert!(all.subsystems.memory && all.subsystems.skills && all.subsystems.voice);
        assert!(all.plugin_enabled("memes") && all.plugin_enabled("scripts"));
        let dev = PersonaManifest::core_only();
        assert!(!dev.subsystems.memory && !dev.subsystems.skills);
        assert!(dev.plugin_enabled("platform_outreach"));
        assert!(!dev.plugin_enabled("memes"));
        assert_eq!(PersonaManifest::builtin_for("dev"), dev);
        assert_eq!(PersonaManifest::builtin_for("default"), all);
    }

    #[test]
    fn parses_partial_toml_and_keeps_the_rest_default() {
        let manifest = PersonaManifest::parse(
            "[subsystems]\nmemory = false\n\n[plugins]\nenabled = [\"ledger\", \"memes\"]\n",
        )
        .unwrap();
        assert!(!manifest.subsystems.memory);
        assert!(manifest.subsystems.skills, "没写的子系统保持默认开");
        assert!(manifest.plugin_enabled("ledger"));
        assert!(!manifest.plugin_enabled("knowledge_base"));
        // 往返:写出来再读回来一致。
        let again = PersonaManifest::parse(&manifest.to_toml()).unwrap();
        assert_eq!(again, manifest);
    }

    #[test]
    fn unknown_plugin_ids_are_rejected() {
        let error = PersonaManifest::parse("[plugins]\nenabled = [\"weather\"]\n").unwrap_err();
        assert!(error.to_string().contains("unknown plugin id"), "{error}");
    }

    #[test]
    fn missing_or_broken_file_falls_back_to_builtin() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let paths = crate::paths::MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("config/fish/conf.d/miyu.fish"),
            bash_hook_file: root.join("config/shell/bash-hook.sh"),
            zsh_hook_file: root.join("config/shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: root.join("system-scripts"),
        };
        let config = AppConfig::default();
        assert_eq!(
            PersonaManifest::load(&config, &paths, "dev"),
            PersonaManifest::core_only()
        );
        let path = PersonaManifest::manifest_path(&config, &paths, "default");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "this is = not toml [").unwrap();
        assert_eq!(
            PersonaManifest::load(&config, &paths, "default"),
            PersonaManifest::all()
        );
        std::fs::write(&path, "[subsystems]\nvoice = false\n").unwrap();
        assert!(
            !PersonaManifest::load(&config, &paths, "default")
                .subsystems
                .voice
        );
    }
}
