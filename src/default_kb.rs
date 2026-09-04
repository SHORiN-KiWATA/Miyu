use crate::config::AppConfig;
use crate::i18n::text as t;
use crate::paths::MiyuPaths;
use crate::tools::knowledge_base::KnowledgeBase;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SHORIN_WIKI_REMOTE: &str = "https://github.com/SHORiN-KiWATA/Shorin-ArchLinux-Guide.git";
const UPDATE_CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;
/// `git ls-remote` 的预算。正常连 GitHub 约 0.4 秒，5 秒是 12 倍余量。
///
/// 有上限这件事本身比数值重要：这条检查在 REPL 启动路径上同步跑，网络黑洞
/// （公司防火墙 DROP、VPN 掉包、强制门户）时 git 自己要 **135 秒**才放弃，
/// 用户看到的就是 `miyu` 启动卡死两分钟。超时了就跳过这轮检查——它只是
/// 「知识库有更新」的提示，不值得挡在提示符前面。
const REMOTE_HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const SPARSE_CHECKOUT_PATTERN: &str = "*.md";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DefaultKbState {
    pub release_hash: String,
    pub shorin_wiki_commit: String,
    pub remote_commit: String,
    pub update_available: bool,
    pub last_checked_at: String,
    pub last_imported_at: String,
    pub last_notice_commit: String,
}

#[derive(Debug, Clone)]
pub struct DefaultKbStatus {
    pub has_update_notice: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UpdateStage {
    CheckingPrerequisites,
    PreparingRepository,
    FetchingRepository,
    CloningRepository,
    CheckingOutRepository,
    ValidatingRepository,
    BuildingSnapshot,
    HashingSnapshot,
    ImportingFiles,
    SavingState,
}

impl UpdateStage {
    pub fn message(self) -> &'static str {
        match self {
            Self::CheckingPrerequisites => {
                t("Checking update prerequisites...", "正在检查更新环境...")
            }
            Self::PreparingRepository => t("Preparing repository cache...", "正在准备仓库缓存..."),
            Self::FetchingRepository => t("Fetching remote updates...", "正在获取远程更新..."),
            Self::CloningRepository => t(
                "Downloading the update repository...",
                "正在下载更新仓库...",
            ),
            Self::CheckingOutRepository => {
                t("Checking out the latest revision...", "正在检出最新版本...")
            }
            Self::ValidatingRepository => {
                t("Validating downloaded content...", "正在校验下载内容...")
            }
            Self::BuildingSnapshot => t(
                "Building the knowledge-base snapshot...",
                "正在整理知识库快照...",
            ),
            Self::HashingSnapshot => t(
                "Calculating the content fingerprint...",
                "正在计算内容校验...",
            ),
            Self::ImportingFiles => t("Importing knowledge-base files...", "正在导入知识库文件..."),
            Self::SavingState => t("Saving update state...", "正在保存更新状态..."),
        }
    }
}

pub fn ensure_initialized(paths: &MiyuPaths, config: &AppConfig) -> Result<()> {
    let source = default_kb_source_dir();
    if !source.is_dir() {
        return Ok(());
    }
    let release_hash = hash_dir(&source)?;
    let state = load_state(paths)?;
    if state.release_hash == release_hash {
        return Ok(());
    }
    import_snapshot(paths, config, &source, &release_hash)
}

pub fn bundled_available() -> bool {
    default_kb_source_dir().is_dir()
}

/// dashboard 用:完整状态(远端提交 / 是否有更新 / 上次导入时间)。
pub fn state(paths: &MiyuPaths) -> Result<DefaultKbState> {
    load_state(paths)
}

pub fn status(paths: &MiyuPaths) -> Result<DefaultKbStatus> {
    let state = load_state(paths)?;
    Ok(DefaultKbStatus {
        has_update_notice: state.update_available
            && !state.remote_commit.is_empty()
            && state.last_notice_commit != state.remote_commit,
    })
}

pub fn notice_if_update_available(paths: &MiyuPaths) -> Result<Option<String>> {
    let mut state = load_state(paths)?;
    if !state.update_available || state.remote_commit.is_empty() {
        return Ok(None);
    }
    if state.last_notice_commit == state.remote_commit {
        return Ok(None);
    }
    let message = t(
        "The default knowledge base needs an update; run miyu update-default-kb",
        "默认知识库需要更新，运行 miyu update-default-kb",
    )
    .to_string();
    state.last_notice_commit = state.remote_commit.clone();
    save_state(paths, &state)?;
    Ok(Some(message))
}

pub async fn check_update_if_due(paths: &MiyuPaths) -> Result<()> {
    let mut state = load_state(paths)?;
    if !should_check(&state) {
        return Ok(());
    }
    state.last_checked_at = Utc::now().to_rfc3339();
    if let Ok(remote) = remote_head().await {
        state.remote_commit = remote.clone();
        state.update_available =
            !state.shorin_wiki_commit.is_empty() && state.shorin_wiki_commit != remote;
    }
    save_state(paths, &state)
}

pub fn update<F>(
    paths: &MiyuPaths,
    config: &AppConfig,
    mut on_progress: F,
) -> Result<DefaultKbState>
where
    F: FnMut(UpdateStage),
{
    on_progress(UpdateStage::CheckingPrerequisites);
    let git = git_command()?;
    let repo = update_repo_dir(paths);
    on_progress(UpdateStage::PreparingRepository);
    cleanup_legacy_update_repo(paths, &repo)?;
    if optimized_update_repo(&git, &repo) {
        on_progress(UpdateStage::FetchingRepository);
        run_git(
            &git,
            &repo,
            &[
                "fetch",
                "--quiet",
                "--depth=1",
                "--filter=blob:none",
                "origin",
                "HEAD",
            ],
        )?;
        on_progress(UpdateStage::CheckingOutRepository);
        run_git(
            &git,
            &repo,
            &[
                "-c",
                "advice.detachedHead=false",
                "checkout",
                "--quiet",
                "--force",
                "FETCH_HEAD",
            ],
        )?;
    } else {
        on_progress(UpdateStage::CloningRepository);
        rebuild_update_repo(&git, &repo, &mut on_progress)?;
    }
    on_progress(UpdateStage::ValidatingRepository);
    validate_update_repo(&repo)?;
    let commit = git_output(&git, &repo, &["rev-parse", "HEAD"])?;
    on_progress(UpdateStage::BuildingSnapshot);
    let source = build_update_source(paths, &repo)?;
    on_progress(UpdateStage::HashingSnapshot);
    let release_hash = hash_dir(&source)?;
    on_progress(UpdateStage::ImportingFiles);
    let kb = KnowledgeBase::new(config.clone(), paths.clone())?;
    kb.replace_default_files(&source)?;
    on_progress(UpdateStage::SavingState);
    let mut state = load_state(paths)?;
    state.release_hash = release_hash;
    state.shorin_wiki_commit = commit.clone();
    state.remote_commit = commit;
    state.update_available = false;
    state.last_checked_at = Utc::now().to_rfc3339();
    state.last_imported_at = Utc::now().to_rfc3339();
    state.last_notice_commit.clear();
    save_state(paths, &state)?;
    Ok(state)
}

fn import_snapshot(
    paths: &MiyuPaths,
    config: &AppConfig,
    source: &Path,
    release_hash: &str,
) -> Result<()> {
    let kb = KnowledgeBase::new(config.clone(), paths.clone())?;
    kb.replace_default_files(source)?;
    let mut state = load_state(paths)?;
    state.release_hash = release_hash.to_string();
    state.shorin_wiki_commit = read_to_string(source.join("manifest/shorinwiki.commit"));
    state.last_imported_at = Utc::now().to_rfc3339();
    save_state(paths, &state)
}

fn default_kb_source_dir() -> PathBuf {
    std::env::var_os("MIYU_DEFAULT_KB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/miyu/default-kb"))
}

fn state_file(paths: &MiyuPaths) -> PathBuf {
    paths.data_dir.join("default-kb/state.json")
}

fn update_repo_dir(paths: &MiyuPaths) -> PathBuf {
    paths
        .cache_dir
        .join("default-kb/shorin-archlinux-guide.git")
}

fn legacy_update_repo_dir(paths: &MiyuPaths) -> PathBuf {
    paths.cache_dir.join("default-kb/shorinwiki.git")
}

fn update_source_dir(paths: &MiyuPaths) -> PathBuf {
    paths.cache_dir.join("default-kb/update-source")
}

fn cleanup_legacy_update_repo(paths: &MiyuPaths, repo: &Path) -> Result<()> {
    let legacy = legacy_update_repo_dir(paths);
    if legacy == repo || !legacy.exists() {
        return Ok(());
    }
    if legacy.join(".git").is_dir() || legacy.is_dir() {
        std::fs::remove_dir_all(legacy)?;
    }
    Ok(())
}

fn optimized_update_repo(git: &str, repo: &Path) -> bool {
    repo.join(".git").is_dir()
        && git_output(git, repo, &["config", "--get", "remote.origin.promisor"])
            .is_ok_and(|value| value == "true")
        && git_output(
            git,
            repo,
            &["config", "--get", "remote.origin.partialclonefilter"],
        )
        .is_ok_and(|value| value == "blob:none")
        && git_output(git, repo, &["config", "--get", "core.sparseCheckout"])
            .is_ok_and(|value| value == "true")
        && git_output(git, repo, &["config", "--get", "core.sparseCheckoutCone"])
            .is_ok_and(|value| value == "false")
        && read_to_string(repo.join(".git/info/sparse-checkout")) == SPARSE_CHECKOUT_PATTERN
}

fn rebuild_update_repo(
    git: &str,
    repo: &Path,
    on_progress: &mut impl FnMut(UpdateStage),
) -> Result<()> {
    let parent = repo.parent().context("update repository has no parent")?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix("shorin-archlinux-guide-")
        .tempdir_in(parent)?;
    let staging_arg = staging.path().display().to_string();
    run_git(
        git,
        parent,
        &[
            "clone",
            "--quiet",
            "--depth=1",
            "--filter=blob:none",
            "--no-checkout",
            SHORIN_WIKI_REMOTE,
            &staging_arg,
        ],
    )?;
    on_progress(UpdateStage::CheckingOutRepository);
    run_git(
        git,
        staging.path(),
        &[
            "sparse-checkout",
            "set",
            "--no-cone",
            SPARSE_CHECKOUT_PATTERN,
        ],
    )?;
    run_git(
        git,
        staging.path(),
        &[
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "--quiet",
            "--force",
            "HEAD",
        ],
    )?;
    validate_update_repo(staging.path())?;
    replace_update_repo(staging.path(), repo)?;
    let _ = staging.keep();
    Ok(())
}

fn replace_update_repo(staging: &Path, repo: &Path) -> Result<()> {
    let backup = repo.with_extension("backup");
    if backup.exists() {
        std::fs::remove_dir_all(&backup)?;
    }
    if repo.exists() {
        std::fs::rename(repo, &backup)?;
    }
    if let Err(err) = std::fs::rename(staging, repo) {
        if backup.exists() {
            std::fs::rename(&backup, repo)
                .context("failed to restore the previous update repository")?;
        }
        return Err(err.into());
    }
    if backup.exists() {
        std::fs::remove_dir_all(backup)?;
    }
    Ok(())
}

fn validate_update_repo(repo: &Path) -> Result<()> {
    let wiki = repo.join("wiki");
    let source = if wiki.is_dir() { wiki.as_path() } else { repo };
    if collect_markdown(source)?
        .iter()
        .all(|file| excluded(file.strip_prefix(source).unwrap_or(file)))
    {
        bail!("default knowledge base update contains no importable Markdown files");
    }
    Ok(())
}

fn load_state(paths: &MiyuPaths) -> Result<DefaultKbState> {
    let path = state_file(paths);
    if !path.is_file() {
        return Ok(DefaultKbState::default());
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

fn save_state(paths: &MiyuPaths, state: &DefaultKbState) -> Result<()> {
    let path = state_file(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

fn should_check(state: &DefaultKbState) -> bool {
    let Ok(last) = chrono::DateTime::parse_from_rfc3339(&state.last_checked_at) else {
        return true;
    };
    Utc::now().timestamp() - last.timestamp() >= UPDATE_CHECK_INTERVAL_SECS
}

async fn remote_head() -> Result<String> {
    remote_head_bounded(SHORIN_WIKI_REMOTE, REMOTE_HEAD_TIMEOUT).await
}

async fn remote_head_bounded(remote: &str, budget: std::time::Duration) -> Result<String> {
    let git = git_command()?;
    // 走 tokio 的 Command 是为了拿 `kill_on_drop`：超时后 future 被丢弃，子进程
    // 跟着被杀，而不是留一个还在等 TCP 的 git 挂在后台。仓库里其它带超时的外部
    // 命令（diagnostics、package_advisor、rg）都是这个写法。
    let output = tokio::time::timeout(
        budget,
        tokio::process::Command::new(git)
            .args(["ls-remote", remote, "HEAD"])
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("git ls-remote timed out"))??;
    if !output.status.success() {
        bail!("git ls-remote failed");
    }
    let text = String::from_utf8(output.stdout)?;
    Ok(text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

fn git_command() -> Result<String> {
    let status = Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(status) if status.success() => Ok("git".to_string()),
        _ => bail!(
            "{}",
            t(
                "Updating the default knowledge base requires git; the installed version remains available",
                "更新默认知识库需要 git；当前继续使用已安装的默认知识库"
            )
        ),
    }
}

fn run_git(git: &str, cwd: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new(git)
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        bail!("git command failed: git {}", args.join(" "));
    }
    Ok(())
}

fn git_output(git: &str, cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(git).current_dir(cwd).args(args).output()?;
    if !output.status.success() {
        bail!("git command failed: git {}", args.join(" "));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn build_update_source(paths: &MiyuPaths, repo: &Path) -> Result<PathBuf> {
    let dest = update_source_dir(paths);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    let bundled = default_kb_source_dir();
    let bundled_kb = bundled.join("kb");
    if bundled_kb.is_dir() {
        copy_markdown_tree(&bundled_kb, &dest.join("kb"))?;
    }
    let wiki = repo.join("wiki");
    let wiki_source = if wiki.is_dir() { wiki.as_path() } else { repo };
    std::fs::create_dir_all(dest.join("shorinwiki"))?;
    for file in collect_markdown(wiki_source)? {
        let rel = file.strip_prefix(wiki_source)?;
        if excluded(rel) {
            continue;
        }
        let target = dest.join("shorinwiki").join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(file, target)?;
    }
    Ok(dest)
}

fn copy_markdown_tree(source: &Path, dest: &Path) -> Result<()> {
    for file in collect_markdown(source)? {
        let rel = file.strip_prefix(source)?;
        if excluded(rel) {
            continue;
        }
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(file, target)?;
    }
    Ok(())
}

fn collect_markdown(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_markdown_inner(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_markdown_inner(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                if matches!(
                    name,
                    ".git" | "pictures" | "legacy" | "Legacy" | "lagacy" | "Lagacy"
                ) {
                    continue;
                }
            }
            collect_markdown_inner(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            files.push(path);
        }
    }
    Ok(())
}

fn excluded(path: &Path) -> bool {
    path.components().any(|component| match component {
        std::path::Component::Normal(name) => matches!(
            name.to_string_lossy().as_ref(),
            ".git" | "pictures" | "legacy" | "Legacy" | "lagacy" | "Lagacy" | "Wikis"
        ),
        _ => false,
    })
}

fn hash_dir(path: &Path) -> Result<String> {
    let mut files = collect_all_files(path)?;
    files.sort();
    let mut hasher = Sha256::new();
    for file in files {
        let rel = file
            .strip_prefix(path)?
            .display()
            .to_string()
            .replace('\\', "/");
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update(std::fs::read(file)?);
        hasher.update([0]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn collect_all_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_all_files_inner(root, &mut files)?;
    Ok(files)
}

fn collect_all_files_inner(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_all_files_inner(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn read_to_string(path: PathBuf) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_progress_has_a_distinct_message_for_every_stage() {
        let stages = [
            UpdateStage::CheckingPrerequisites,
            UpdateStage::PreparingRepository,
            UpdateStage::FetchingRepository,
            UpdateStage::CloningRepository,
            UpdateStage::CheckingOutRepository,
            UpdateStage::ValidatingRepository,
            UpdateStage::BuildingSnapshot,
            UpdateStage::HashingSnapshot,
            UpdateStage::ImportingFiles,
            UpdateStage::SavingState,
        ];
        let messages = stages.map(UpdateStage::message);

        assert!(messages.iter().all(|message| !message.trim().is_empty()));
        let unique = messages
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), stages.len());
    }

    #[test]
    fn update_repo_requires_importable_markdown() {
        let temp = tempfile::tempdir().unwrap();
        let wiki = temp.path().join("wiki");
        std::fs::create_dir_all(wiki.join("legacy")).unwrap();
        std::fs::write(wiki.join("legacy/old.md"), "old").unwrap();

        assert!(validate_update_repo(temp.path()).is_err());

        std::fs::create_dir_all(wiki.join("archlinux")).unwrap();
        std::fs::write(wiki.join("archlinux/current.md"), "current").unwrap();

        assert!(validate_update_repo(temp.path()).is_ok());
    }

    #[test]
    fn replacing_update_repo_removes_previous_cache() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo.git");
        let staging = temp.path().join("staging");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(repo.join("old"), "old").unwrap();
        std::fs::write(staging.join("new"), "new").unwrap();

        replace_update_repo(&staging, &repo).unwrap();

        assert_eq!(std::fs::read_to_string(repo.join("new")).unwrap(), "new");
        assert!(!repo.join("old").exists());
        assert!(!repo.with_extension("backup").exists());
    }
}

#[cfg(test)]
mod remote_head_tests {
    use super::*;

    /// 10.255.255.1 是 RFC1918 里一个不会有人应答的地址，`git ls-remote` 打过去
    /// 会一直等 TCP——实测 git 自己要 **135 秒**才放弃。这条检查在 REPL 启动路径
    /// 上，所以必须有上限。
    ///
    /// 用 200 ms 预算测，跑得比一次 `cargo test` 的启动还快。断言只看「有没有
    /// 被上限兜住」，不看具体返回什么：没网的环境里 connect 会立刻
    /// EHOSTUNREACH，照样是「很快返回」，测试不会假红。
    #[tokio::test]
    async fn remote_head_gives_up_instead_of_hanging() {
        let budget = std::time::Duration::from_millis(200);
        let started = std::time::Instant::now();
        let result = remote_head_bounded("https://10.255.255.1/nope.git", budget).await;
        let waited = started.elapsed();
        assert!(result.is_err(), "黑洞地址不该返回成功");
        assert!(
            waited < std::time::Duration::from_secs(3),
            "等了 {waited:?}，超时没兜住"
        );
    }
}
