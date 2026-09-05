//! 脚本工具的测试。

mod bundled;
mod dashboard;
mod header;
mod index;
mod manage;
mod run;

use crate::config::AppConfig;
use crate::paths::MiyuPaths;
use std::path::Path;

/// 隔离的路径集:用户层在 `<root>/data/scripts`(persona 子层
/// `personas/default`),内置层在 `<root>/system`。
pub(super) fn test_env(root: &Path) -> (AppConfig, MiyuPaths) {
    let mut paths = MiyuPaths::new().unwrap();
    paths.scripts_dir = root.join("data/scripts");
    paths.system_scripts_dir = root.join("system");
    paths.cache_dir = root.join("cache");
    std::fs::create_dir_all(&paths.scripts_dir).unwrap();
    (AppConfig::default(), paths)
}

pub(super) fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}
