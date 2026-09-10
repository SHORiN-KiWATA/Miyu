//! 配置里的路径解析与历史布局迁移。
//!
//! 配置文件里存的是**相对路径**，运行时再拼上 MIYU_HOME——绝对路径会让配置没
//! 法跨机器复制。`normalize_relative_path` 负责统一写法，`config_relative_path`
//! 是反向。
//!
//! `migrated_resource_path` / `fallback_resource_file` 处理的是资源目录换过位置
//! 的历史包袱：老配置指向旧路径，读的时候按新旧两处找。

use crate::config::*;

pub(crate) fn normalized_relative_path(value: &str) -> Option<PathBuf> {
    normalize_relative_path(Path::new(value.trim()))
}

pub(crate) fn normalize_relative_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(component) => normalized.push(component),
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

pub(crate) fn relative_path_equals(value: &str, expected: &str) -> bool {
    normalized_relative_path(value).as_deref() == Some(Path::new(expected))
}

pub(crate) fn migrated_resource_path(paths: &MiyuPaths, value: &str) -> Option<PathBuf> {
    paths.migrated_resource_path(Path::new(value.trim()))
}

pub(crate) fn fallback_resource_file(
    paths: &MiyuPaths,
    namespace: &str,
    file_name: &str,
) -> PathBuf {
    // 家目录布局:属主档案是 home/<admin>/profile.md,其余身份文件跟进家目录。
    if namespace == "identities" {
        if paths.admin_home_dir().is_some() {
            if file_name == "user-identity.md" {
                return paths.profile_file();
            }
            return paths.identities_dir().join(file_name);
        }
    }
    if paths.resources_use_config_dir() {
        paths.config_dir.join(file_name)
    } else {
        paths.resource_dir().join(namespace).join(file_name)
    }
}

pub(crate) fn migrated_fallback_file(
    paths: &MiyuPaths,
    value: &str,
    namespace: &str,
    file_name: &str,
) -> Option<PathBuf> {
    let path = Path::new(value.trim());
    let matches_current = path == paths.config_dir.join(file_name);
    let matches_legacy = paths
        .legacy_config_dir()
        .is_some_and(|legacy| path == legacy.join(file_name));
    (path.is_absolute() && (matches_current || matches_legacy))
        .then(|| fallback_resource_file(paths, namespace, file_name))
}

pub(crate) fn config_relative_path(paths: &MiyuPaths, value: &str) -> PathBuf {
    let path = PathBuf::from(value.trim());
    if path.is_absolute() {
        path
    } else {
        paths.config_dir.join(path)
    }
}

pub(crate) fn persona_scope_name(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "default".to_string();
    }
    let normalized = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if normalized.is_empty() {
        format!("persona-{}", &blake3::hash(name.as_bytes()).to_hex()[..12])
    } else {
        normalized
    }
}
