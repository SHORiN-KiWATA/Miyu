use crate::config::{PlatformCommandPermission, PlatformsConfig};
use crate::i18n::text as t;

pub(crate) const RESET_COMMAND_ID: &str = "reset";
pub(crate) const WIPE_COMMAND_ID: &str = "wipe";
pub(crate) const RESET_MEMORY_COMMAND_ID: &str = "reset-memory";
pub(crate) const RESET_ALL_MEMORY_COMMAND_ID: &str = "reset-all-memory";
pub(crate) const STOP_COMMAND_ID: &str = "stop";
pub(crate) const MODELS_COMMAND_ID: &str = "models";

#[derive(Clone, Copy, Debug)]
pub(crate) struct PlatformCommandDescriptor {
    pub(crate) id: &'static str,
    pub(crate) default_permission: PlatformCommandPermission,
}

pub(crate) const BUILTIN_COMMANDS: &[PlatformCommandDescriptor] = &[
    PlatformCommandDescriptor {
        id: RESET_COMMAND_ID,
        default_permission: PlatformCommandPermission::AdminOnly,
    },
    // Deliberately its own descriptor: permission used to be granted for
    // "reset" as a whole, so opening `/reset` up to a group — a reasonable
    // thing to want — handed everyone the memory wipe as well.
    PlatformCommandDescriptor {
        id: WIPE_COMMAND_ID,
        default_permission: PlatformCommandPermission::AdminOnly,
    },
    // 只清长期记忆,会话/技能不动;与 wipe 同款独立描述符+confirm 字面。
    PlatformCommandDescriptor {
        id: RESET_MEMORY_COMMAND_ID,
        default_permission: PlatformCommandPermission::AdminOnly,
    },
    // 全量那条单独一个描述符:把"清本会话"开给群友是合理的,那不该顺带
    // 把整个人格的记忆也交出去(wipe 与 reset 分家同一个理由)。
    PlatformCommandDescriptor {
        id: RESET_ALL_MEMORY_COMMAND_ID,
        default_permission: PlatformCommandPermission::AdminOnly,
    },
    PlatformCommandDescriptor {
        id: STOP_COMMAND_ID,
        default_permission: PlatformCommandPermission::AdminOnly,
    },
    PlatformCommandDescriptor {
        id: MODELS_COMMAND_ID,
        default_permission: PlatformCommandPermission::AdminOnly,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResetScope {
    Current,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParsedPlatformCommand {
    Reset {
        scope: Option<ResetScope>,
    },
    /// `confirmed` is the literal `confirm` argument. A destructive command on
    /// a chat platform gets no dialog box, so the confirmation is the word
    /// itself — stateless, and impossible to hit by muscle memory.
    Wipe {
        confirmed: bool,
    },
    ResetMemory,
    ResetAllMemory,
    Stop {
        has_arguments: bool,
    },
    Models {
        argument: Option<String>,
    },
}

pub(crate) fn descriptor(id: &str) -> Option<&'static PlatformCommandDescriptor> {
    BUILTIN_COMMANDS.iter().find(|command| command.id == id)
}

pub(crate) fn parse(config: &PlatformsConfig, text: &str) -> Option<ParsedPlatformCommand> {
    let text = text.trim();
    let rest = text.strip_prefix(&config.command_prefix)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        return None;
    }

    let mut parts = rest.split_whitespace();
    let command = parts.next().unwrap_or_default();
    if command.eq_ignore_ascii_case(RESET_COMMAND_ID) {
        let scope = match (parts.next(), parts.next()) {
            (None, None) => Some(ResetScope::Current),
            _ => None,
        };
        Some(ParsedPlatformCommand::Reset { scope })
    } else if command.eq_ignore_ascii_case(WIPE_COMMAND_ID) {
        let confirmed = match (parts.next(), parts.next()) {
            (None, None) => false,
            (Some(argument), None) if argument.eq_ignore_ascii_case("confirm") => true,
            _ => return None,
        };
        Some(ParsedPlatformCommand::Wipe { confirmed })
    } else if command.eq_ignore_ascii_case(RESET_ALL_MEMORY_COMMAND_ID) {
        // 与 reset-memory 同款 confirm 处理:老习惯打出来不该被当成聊天。
        // 顺序在前:两个名字都以 `reset-` 开头,但比对是全名相等,先后其实
        // 无所谓——放这里只是让两条相邻好读。
        match (parts.next(), parts.next()) {
            (None, None) => {}
            (Some(argument), None) if argument.eq_ignore_ascii_case("confirm") => {}
            _ => return None,
        }
        Some(ParsedPlatformCommand::ResetAllMemory)
    } else if command.eq_ignore_ascii_case(RESET_MEMORY_COMMAND_ID) {
        // 不再要二次确认(与本地三条路一致):这条命令本来就是 AdminOnly,
        // 清的只是长期记忆,会话历史/技能/知识库都不动。仍然收下 `confirm`
        // 这个词——老习惯打出来不该被当成普通聊天发给模型。
        match (parts.next(), parts.next()) {
            (None, None) => {}
            (Some(argument), None) if argument.eq_ignore_ascii_case("confirm") => {}
            _ => return None,
        }
        Some(ParsedPlatformCommand::ResetMemory)
    } else if command.eq_ignore_ascii_case(STOP_COMMAND_ID) {
        let has_arguments = parts.next().is_some();
        Some(ParsedPlatformCommand::Stop { has_arguments })
    } else if command.eq_ignore_ascii_case(MODELS_COMMAND_ID) {
        let argument = rest
            .split_once(char::is_whitespace)
            .map(|(_, argument)| argument.trim().to_string())
            .filter(|argument| !argument.is_empty());
        Some(ParsedPlatformCommand::Models { argument })
    } else {
        None
    }
}

pub(crate) fn is_allowed(
    config: &PlatformsConfig,
    command: &PlatformCommandDescriptor,
    is_admin: bool,
) -> bool {
    match config.command_permission(command.id, command.default_permission) {
        PlatformCommandPermission::Everyone => true,
        PlatformCommandPermission::AdminOnly => is_admin,
    }
}

pub(crate) fn command_text(config: &PlatformsConfig, command: &str) -> String {
    format!("{}{}", config.command_prefix, command)
}

pub(crate) fn reset_usage_message(config: &PlatformsConfig) -> String {
    usage_message(config, RESET_COMMAND_ID)
}

pub(crate) fn wipe_confirm_message(config: &PlatformsConfig) -> String {
    let wipe = command_text(config, WIPE_COMMAND_ID);
    t_owned(
        format!(
            "This erases memory, every conversation's contents, group-chat contexts and auto-generated skills, and cannot be undone. Send `{wipe} confirm` to go ahead."
        ),
        format!(
            "这会抹掉记忆、所有会话的内容、群聊上下文和自动生成的技能，不可撤销。确认请发送 `{wipe} confirm`。"
        ),
    )
}

fn t_owned(en: String, zh: String) -> String {
    if t("en", "zh") == "zh" {
        zh
    } else {
        en
    }
}

pub(crate) fn stop_usage_message(config: &PlatformsConfig) -> String {
    usage_message(config, STOP_COMMAND_ID)
}

pub(crate) fn models_switch_hint(config: &PlatformsConfig) -> String {
    format!(
        "{} <{}>",
        command_text(config, MODELS_COMMAND_ID),
        t("index or provider/model", "序号或 供应商/模型")
    )
}

fn usage_message(config: &PlatformsConfig, command: &str) -> String {
    format!(
        "{}{}{}",
        t("Usage", "用法"),
        t(": ", "："),
        command_text(config, command)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PlatformCommandConfig, PlatformsConfig};

    #[test]
    fn wipe_needs_the_word_confirm_and_has_its_own_permission() {
        let config = PlatformsConfig::default();
        assert_eq!(
            parse(&config, "/wipe"),
            Some(ParsedPlatformCommand::Wipe { confirmed: false })
        );
        assert_eq!(
            parse(&config, "/wipe confirm"),
            Some(ParsedPlatformCommand::Wipe { confirmed: true })
        );
        assert_eq!(parse(&config, "/wipe now"), None);
        // 不再需要二次确认;`confirm` 仍然收下,免得老习惯打出来被当成聊天。
        assert_eq!(
            parse(&config, "/reset-memory"),
            Some(ParsedPlatformCommand::ResetMemory)
        );
        assert_eq!(
            parse(&config, "/reset-memory confirm"),
            Some(ParsedPlatformCommand::ResetMemory)
        );
        assert_eq!(parse(&config, "/reset-memory now"), None);
        // 全量那条是另一条命令,不能被 `/reset-memory` 的前缀比对吞掉。
        assert_eq!(
            parse(&config, "/reset-all-memory"),
            Some(ParsedPlatformCommand::ResetAllMemory)
        );
        assert_eq!(
            parse(&config, "/reset-all-memory confirm"),
            Some(ParsedPlatformCommand::ResetAllMemory)
        );
        assert_eq!(parse(&config, "/reset-all-memory now"), None);
        let reset_all = descriptor(RESET_ALL_MEMORY_COMMAND_ID).unwrap();
        assert_eq!(
            reset_all.default_permission,
            PlatformCommandPermission::AdminOnly
        );
        assert!(!is_allowed(&config, reset_all, false));

        // Opening `/reset` up to a group used to hand out the memory wipe with
        // it, because both scopes shared one descriptor.
        let reset = descriptor(RESET_COMMAND_ID).unwrap();
        let wipe = descriptor(WIPE_COMMAND_ID).unwrap();
        assert_ne!(reset.id, wipe.id);
        assert_eq!(
            wipe.default_permission,
            PlatformCommandPermission::AdminOnly
        );
        assert!(!is_allowed(&config, wipe, false));
        assert!(is_allowed(&config, wipe, true));
    }

    #[test]
    fn parses_default_and_custom_prefixes_with_command_boundaries() {
        let mut config = PlatformsConfig::default();
        assert_eq!(
            parse(&config, "/reset"),
            Some(ParsedPlatformCommand::Reset {
                scope: Some(ResetScope::Current)
            })
        );
        assert_eq!(
            parse(&config, "  /RESET  "),
            Some(ParsedPlatformCommand::Reset {
                scope: Some(ResetScope::Current)
            })
        );
        assert_eq!(
            parse(&config, "/reset all"),
            Some(ParsedPlatformCommand::Reset { scope: None })
        );
        assert_eq!(
            parse(&config, "/reset now"),
            Some(ParsedPlatformCommand::Reset { scope: None })
        );
        assert_eq!(
            parse(&config, "/reset all extra"),
            Some(ParsedPlatformCommand::Reset { scope: None })
        );
        assert_eq!(parse(&config, "/resetting"), None);
        assert_eq!(
            parse(&config, "/STOP"),
            Some(ParsedPlatformCommand::Stop {
                has_arguments: false
            })
        );
        assert_eq!(
            parse(&config, "/stop now"),
            Some(ParsedPlatformCommand::Stop {
                has_arguments: true
            })
        );
        assert_eq!(parse(&config, "/stopping"), None);
        assert_eq!(
            parse(&config, "/models"),
            Some(ParsedPlatformCommand::Models { argument: None })
        );
        assert_eq!(
            parse(&config, "/MODELS  3 "),
            Some(ParsedPlatformCommand::Models {
                argument: Some("3".to_string())
            })
        );
        assert_eq!(
            parse(&config, "/models openai/gpt-5.2"),
            Some(ParsedPlatformCommand::Models {
                argument: Some("openai/gpt-5.2".to_string())
            })
        );
        assert_eq!(parse(&config, "/modelsx"), None);
        assert_eq!(parse(&config, "/missing"), None);
        assert_eq!(parse(&config, "/ reset"), None);
        assert_eq!(parse(&config, "/"), None);
        assert_eq!(parse(&config, "please /reset"), None);

        config.command_prefix = "喵".to_string();
        assert_eq!(
            parse(&config, "喵reset"),
            Some(ParsedPlatformCommand::Reset {
                scope: Some(ResetScope::Current)
            })
        );
        assert_eq!(parse(&config, "/reset"), None);
    }

    #[test]
    fn control_commands_default_to_admin_and_support_an_everyone_override() {
        let mut config = PlatformsConfig::default();
        let reset = descriptor(RESET_COMMAND_ID).unwrap();
        let stop = descriptor(STOP_COMMAND_ID).unwrap();
        let models = descriptor(MODELS_COMMAND_ID).unwrap();
        assert!(is_allowed(&config, reset, true));
        assert!(!is_allowed(&config, reset, false));
        assert!(is_allowed(&config, stop, true));
        assert!(!is_allowed(&config, stop, false));
        assert!(is_allowed(&config, models, true));
        assert!(!is_allowed(&config, models, false));

        config.commands.insert(
            RESET_COMMAND_ID.to_string(),
            PlatformCommandConfig {
                permission: PlatformCommandPermission::Everyone,
            },
        );
        assert!(is_allowed(&config, reset, false));
        config.commands.insert(
            STOP_COMMAND_ID.to_string(),
            PlatformCommandConfig {
                permission: PlatformCommandPermission::Everyone,
            },
        );
        assert!(is_allowed(&config, stop, false));
    }
}
