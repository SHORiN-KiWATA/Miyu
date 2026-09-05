//! 脚本目录的变更检测:指纹没变就不重扫。
//!
//! 此前每个工具回合都在持 registry 锁的状态下同步全量重扫四个目录——每个
//! 脚本 `read_to_string` 读全文(内置脚本合计 500KB)再 canonicalize。这里
//! 照 skills 的做法:目录清单 + 每个文件的元数据(长度/mtime/inode)做 blake3
//! 指纹,变了才扫;扫描本身放 spawn_blocking,读完再短暂持锁替换。三次稳定读
//! 防扫描途中目录还在变。

use crate::tools::scripts::*;

pub(crate) struct ScriptRefreshSnapshot {
    pub(crate) scan: ScriptScanResult,
    pub(crate) fingerprint: [u8; 32],
}

pub(crate) fn catalog_fingerprint(roots: &[PathBuf]) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    for root in roots {
        hasher.update(root.as_os_str().as_encoded_bytes());
        let Ok(read_dir) = std::fs::read_dir(root) else {
            hasher.update(&[0]);
            continue;
        };
        let mut paths: Vec<PathBuf> = read_dir
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect();
        paths.sort();
        for path in paths {
            hasher.update(path.as_os_str().as_encoded_bytes());
            crate::skills::hash_metadata(&mut hasher, &path)?;
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

/// 指纹与 `current` 一致 → None(什么都不用做);否则返回稳定态的扫描结果。
pub(crate) fn prepare_script_refresh(
    current: Option<[u8; 32]>,
    config: &crate::config::AppConfig,
    paths: &MiyuPaths,
) -> Result<Option<ScriptRefreshSnapshot>> {
    let roots = script_scan_roots(config, paths);
    for _ in 0..3 {
        let before = catalog_fingerprint(&roots)?;
        if Some(before) == current {
            return Ok(None);
        }
        let dirs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
        let scan = scan_scripts(&dirs)?;
        let after = catalog_fingerprint(&roots)?;
        if before == after {
            return Ok(Some(ScriptRefreshSnapshot {
                scan,
                fingerprint: after,
            }));
        }
    }
    bail!("script directories kept changing while they were being scanned")
}

pub(crate) fn apply_script_refresh(
    registry: &mut ToolRegistry,
    paths: &MiyuPaths,
    snapshot: ScriptRefreshSnapshot,
) {
    let specs = script_specs(&snapshot.scan.entries, &paths.scripts_dir, &paths.cache_dir);
    if let Err(error) = registry.replace_script_tools(specs, snapshot.scan.unregistered) {
        tracing::warn!(error = %error, "failed to replace Miyu script tools");
    }
    // 失败也记指纹:同一份坏目录不值得每回合重扫一次,改了文件指纹自然变。
    registry.set_script_catalog_fingerprint(snapshot.fingerprint);
}
