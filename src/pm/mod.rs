//! 包管理器 `miyu pm`(09-10 分层架构阶段 7)。
//!
//! 「插件」不是第五种运行时,是一个清单捆绑包:一个 git 仓库(或本地目录),根上
//! 一份 `miyu-package.toml`,里面说自己带了哪些脚本、技能,或者整个是一个人格。
//! 装包只往两个地方写:`extensions/`(脚本/技能)与 `personas/`(人格清单),
//! 外加人格的提示词与头像(它们今天还住 `data/prompts`、`data/persona-avatars`)。
//! 每个装进来的文件都记在锁文件里,卸载按锁文件删,升级 = 卸了再装。
//!
//! 索引:tap 是一个 GitHub 仓库,根上 `index.json` 把包名映射到 `owner/repo`;
//! 官方 tap 缺省在列,`miyu pm tap add owner/repo` 加第三方。`install` 也接受
//! `owner/repo[@ref]`、GitHub URL 或本地路径,不经索引。
//!
//! 只做「防君子」的校验:清单合法、`requires-miyu` 满足、目标文件不撞别的包。
//! 不做签名、不做沙盒。

use crate::config::persona_scope_name;
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

pub const MANIFEST_FILE: &str = "miyu-package.toml";
pub const OFFICIAL_TAP: &str = "SHORiN-KiWATA/miyu-packages";
const LOCK_FILE: &str = "lock.json";
const TAPS_FILE: &str = "taps.json";
const INDEX_FILE: &str = "index.json";
const MAX_PACKAGE_FILES: usize = 2_000;
const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;

// ── 清单 ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageKind {
    /// 脚本 + 技能,装进全局层(所有人格可见)。
    Extension,
    /// 一个人格:提示词、清单、头像,以及只给这个人格的脚本/技能。
    Persona,
}

impl PackageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Persona => "persona",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub package: PackageSection,
    #[serde(default)]
    pub install: InstallSection,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PackageSection {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_kind")]
    pub kind: PackageKind,
    /// 如 `>=0.5.0`;只支持 `>=`(缺省也是 `>=`)。
    #[serde(default)]
    pub requires_miyu: String,
}

fn default_kind() -> PackageKind {
    PackageKind::Extension
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct InstallSection {
    /// 脚本文件的 glob(相对包根;只匹配文件),缺省 `scripts/*`。
    #[serde(default)]
    pub scripts: Option<Vec<String>>,
    /// 技能目录的 glob(每个目录里要有 SKILL.md),缺省 `skills/*`。
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    /// 人格目录(`persona.md` 必有;可选 `persona.json`、`persona.toml`、`assets/`),
    /// 缺省 `persona`。只对 kind = persona 有意义。
    #[serde(default)]
    pub persona: Option<String>,
}

pub fn validate_package_name(name: &str) -> Result<()> {
    let count = name.chars().count();
    if !(2..=64).contains(&count) {
        bail!("package name must be 2 to 64 characters: {name:?}");
    }
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
    {
        bail!("package name may only contain lowercase letters, digits, '-' and '_' and must start with a letter or digit: {name:?}");
    }
    Ok(())
}

impl PackageManifest {
    pub fn parse(raw: &str) -> Result<Self> {
        let manifest: Self = toml::from_str(raw).context("parsing miyu-package.toml")?;
        validate_package_name(&manifest.package.name)?;
        if !manifest.package.requires_miyu.trim().is_empty() {
            parse_requirement(&manifest.package.requires_miyu)?;
        }
        Ok(manifest)
    }

    pub fn load(package_root: &Path) -> Result<Self> {
        let path = package_root.join(MANIFEST_FILE);
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&raw)
    }

    /// `requires-miyu` 对当前二进制是否满足。
    pub fn check_requirement(&self) -> Result<()> {
        let spec = self.package.requires_miyu.trim();
        if spec.is_empty() {
            return Ok(());
        }
        let required = parse_requirement(spec)?;
        let current = parse_version(env!("CARGO_PKG_VERSION"))?;
        if current < required {
            bail!(
                "package {} requires miyu >= {}.{}.{}, this is {}",
                self.package.name,
                required.0,
                required.1,
                required.2,
                env!("CARGO_PKG_VERSION")
            );
        }
        Ok(())
    }
}

fn parse_requirement(spec: &str) -> Result<(u64, u64, u64)> {
    let trimmed = spec.trim();
    let version = trimmed
        .strip_prefix(">=")
        .or_else(|| trimmed.strip_prefix('^'))
        .unwrap_or(trimmed)
        .trim();
    parse_version(version).with_context(|| format!("invalid requires-miyu: {spec:?}"))
}

fn parse_version(value: &str) -> Result<(u64, u64, u64)> {
    let core = value.trim().split(['-', '+']).next().unwrap_or_default();
    let mut parts = core.split('.');
    let mut next = || -> Result<u64> {
        match parts.next() {
            None | Some("") => Ok(0),
            Some(part) => part
                .parse::<u64>()
                .with_context(|| format!("invalid version component {part:?} in {value:?}")),
        }
    };
    let major = next()?;
    let minor = next()?;
    let patch = next()?;
    if parts.next().is_some() {
        bail!("invalid version {value:?}");
    }
    Ok((major, minor, patch))
}

// ── 锁文件 / tap ──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LockFile {
    #[serde(default)]
    pub packages: BTreeMap<String, InstalledPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackage {
    /// 装的时候给的来源:`owner/repo`、URL 或本地路径。
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// git commit(拿得到的话)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default)]
    pub version: String,
    pub kind: PackageKind,
    #[serde(default)]
    pub description: String,
    pub installed_at: String,
    /// 装进来的文件,相对 `MIYU_HOME` 根。卸载按这个删。
    pub files: Vec<String>,
    /// 全部文件内容的 blake3;升级时与新内容比,相同就不动。
    pub fingerprint: String,
}

pub fn pm_dir(paths: &MiyuPaths) -> PathBuf {
    match paths.extensions_dir() {
        Some(extensions) => extensions.join("pm"),
        None => paths.data_dir.join("pm"),
    }
}

pub fn load_lock(paths: &MiyuPaths) -> Result<LockFile> {
    let path = pm_dir(paths).join(LOCK_FILE);
    match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("parsing package lock {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LockFile::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save_lock(paths: &MiyuPaths, lock: &LockFile) -> Result<()> {
    let dir = pm_dir(paths);
    fs::create_dir_all(&dir)?;
    let path = dir.join(LOCK_FILE);
    let temporary = dir.join(format!(".{LOCK_FILE}.tmp-{}", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(lock)?)?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

pub fn load_taps(paths: &MiyuPaths) -> Result<Vec<String>> {
    let path = pm_dir(paths).join(TAPS_FILE);
    let mut taps: Vec<String> = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("parsing taps {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if !taps.iter().any(|tap| tap == OFFICIAL_TAP) {
        taps.insert(0, OFFICIAL_TAP.to_string());
    }
    Ok(taps)
}

pub fn save_taps(paths: &MiyuPaths, taps: &[String]) -> Result<()> {
    let dir = pm_dir(paths);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(TAPS_FILE), serde_json::to_vec_pretty(taps)?)?;
    Ok(())
}

pub fn validate_repo_slug(slug: &str) -> Result<(String, String)> {
    let (owner, repo) = slug
        .trim()
        .trim_end_matches('/')
        .split_once('/')
        .with_context(|| format!("expected owner/repo, got {slug:?}"))?;
    let ok = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && part != "."
            && part != ".."
    };
    if !ok(owner) || !ok(repo) || repo.contains('/') {
        bail!("invalid repository slug {slug:?}");
    }
    Ok((owner.to_string(), repo.trim_end_matches(".git").to_string()))
}

// ── 来源 ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    Local(PathBuf),
    GitHub {
        owner: String,
        repo: String,
        reference: Option<String>,
    },
}

impl PackageSource {
    pub fn describe(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::GitHub { owner, repo, .. } => format!("{owner}/{repo}"),
        }
    }

    pub fn reference(&self) -> Option<String> {
        match self {
            Self::Local(_) => None,
            Self::GitHub { reference, .. } => reference.clone(),
        }
    }

    /// `./dir`、`/abs`、`owner/repo[@ref]`、`https://github.com/owner/repo[/tree/ref]`。
    /// 裸包名不在这里解析(要查索引)。
    pub fn parse_spec(spec: &str, reference: Option<&str>) -> Result<Option<Self>> {
        let spec = spec.trim();
        if spec.is_empty() {
            bail!("empty package spec");
        }
        let as_path = Path::new(spec);
        if spec.starts_with('.') || spec.starts_with('/') || spec.starts_with('~') {
            let path = if let Some(rest) = spec.strip_prefix("~/") {
                directories::BaseDirs::new()
                    .map(|dirs| dirs.home_dir().join(rest))
                    .unwrap_or_else(|| as_path.to_path_buf())
            } else {
                as_path.to_path_buf()
            };
            let canonical = fs::canonicalize(&path)
                .with_context(|| format!("package directory not found: {}", path.display()))?;
            return Ok(Some(Self::Local(canonical)));
        }
        if let Some(rest) = spec
            .strip_prefix("https://github.com/")
            .or_else(|| spec.strip_prefix("http://github.com/"))
            .or_else(|| spec.strip_prefix("github.com/"))
        {
            let mut parts = rest.trim_end_matches('/').splitn(4, '/');
            let owner = parts.next().unwrap_or_default();
            let repo = parts.next().unwrap_or_default();
            let (owner, repo) = validate_repo_slug(&format!("{owner}/{repo}"))?;
            let url_ref = match (parts.next(), parts.next()) {
                (Some("tree") | Some("commit"), Some(reference)) if !reference.is_empty() => {
                    Some(reference.to_string())
                }
                _ => None,
            };
            return Ok(Some(Self::GitHub {
                owner,
                repo,
                reference: reference.map(str::to_string).or(url_ref),
            }));
        }
        if spec.contains('/') {
            let (slug, at_ref) = match spec.split_once('@') {
                Some((slug, reference)) if !reference.is_empty() => (slug, Some(reference)),
                _ => (spec, None),
            };
            let (owner, repo) = validate_repo_slug(slug)?;
            return Ok(Some(Self::GitHub {
                owner,
                repo,
                reference: reference.or(at_ref).map(str::to_string),
            }));
        }
        Ok(None)
    }
}

/// tap 仓库根上的 `index.json`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TapIndex {
    #[serde(default)]
    pub packages: BTreeMap<String, TapEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapEntry {
    /// `owner/repo`
    pub repo: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: Option<PackageKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(concat!("miyu-pm/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

pub async fn fetch_tap_index(tap: &str) -> Result<TapIndex> {
    let (owner, repo) = validate_repo_slug(tap)?;
    let url = format!("https://raw.githubusercontent.com/{owner}/{repo}/HEAD/{INDEX_FILE}");
    let response = http_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching tap index {url}"))?;
    if !response.status().is_success() {
        bail!(
            "tap {tap} has no readable {INDEX_FILE} ({})",
            response.status()
        );
    }
    let raw = response.text().await?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {INDEX_FILE} of tap {tap}"))
}

/// 在所有 tap 里找一个包名;先命中的 tap 赢(官方 tap 排最前)。
pub async fn resolve_from_taps(paths: &MiyuPaths, name: &str) -> Result<PackageSource> {
    validate_package_name(name)?;
    let taps = load_taps(paths)?;
    let mut errors = Vec::new();
    for tap in &taps {
        match fetch_tap_index(tap).await {
            Ok(index) => {
                if let Some(entry) = index.packages.get(name) {
                    let (owner, repo) = validate_repo_slug(&entry.repo)?;
                    return Ok(PackageSource::GitHub {
                        owner,
                        repo,
                        reference: entry.reference.clone(),
                    });
                }
            }
            Err(error) => errors.push(format!("{tap}: {error:#}")),
        }
    }
    if errors.is_empty() {
        bail!("package {name:?} is not in any tap ({})", taps.join(", "));
    }
    bail!(
        "package {name:?} not found; some taps could not be read: {}",
        errors.join("; ")
    );
}

/// 拉到本地一个临时目录,返回(包根, commit)。
pub struct FetchedPackage {
    pub root: PathBuf,
    pub commit: Option<String>,
    _temp: Option<tempfile::TempDir>,
}

pub async fn fetch(source: &PackageSource) -> Result<FetchedPackage> {
    match source {
        PackageSource::Local(path) => {
            if !path.join(MANIFEST_FILE).is_file() {
                bail!("{} has no {MANIFEST_FILE}", path.display());
            }
            Ok(FetchedPackage {
                root: path.clone(),
                commit: local_git_commit(path),
                _temp: None,
            })
        }
        PackageSource::GitHub {
            owner,
            repo,
            reference,
        } => {
            let reference = reference.as_deref().unwrap_or("HEAD");
            let url = format!("https://codeload.github.com/{owner}/{repo}/tar.gz/{reference}");
            let response = http_client()?
                .get(&url)
                .send()
                .await
                .with_context(|| format!("downloading {url}"))?;
            if !response.status().is_success() {
                bail!(
                    "cannot download {owner}/{repo}@{reference} ({})",
                    response.status()
                );
            }
            let bytes = response.bytes().await?;
            if bytes.len() as u64 > MAX_PACKAGE_BYTES {
                bail!("package archive exceeds {MAX_PACKAGE_BYTES} bytes");
            }
            let temp = tempfile::tempdir().context("creating a temporary directory")?;
            let root = extract_tarball(&bytes, temp.path())?;
            let commit = github_commit(owner, repo, reference).await;
            Ok(FetchedPackage {
                root,
                commit,
                _temp: Some(temp),
            })
        }
    }
}

fn local_git_commit(path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", &path.display().to_string(), "rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (sha.len() >= 7).then_some(sha)
}

async fn github_commit(owner: &str, repo: &str, reference: &str) -> Option<String> {
    let url = format!("https://github.com/{owner}/{repo}");
    let output = tokio::process::Command::new("git")
        .args(["ls-remote", &url, reference])
        .output()
        .await
        .ok()?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        if let Some(sha) = text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().next())
        {
            if sha.len() >= 7 {
                return Some(sha.to_string());
            }
        }
    }
    let api = format!("https://api.github.com/repos/{owner}/{repo}/commits/{reference}");
    let response = http_client()
        .ok()?
        .get(&api)
        .header("accept", "application/vnd.github.sha")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let sha = response.text().await.ok()?.trim().to_string();
    (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())).then_some(sha)
}

/// codeload 的 tar.gz 顶层是 `<repo>-<ref>/`;解到临时目录后返回那个目录。
/// 路径里带 `..` 或绝对路径的条目一律拒绝。
fn extract_tarball(bytes: &[u8], into: &Path) -> Result<PathBuf> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut top: Option<PathBuf> = None;
    let mut count = 0usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            bail!("archive entry escapes the package: {}", path.display());
        }
        if entry.header().entry_type().is_symlink() || entry.header().entry_type().is_hard_link() {
            continue;
        }
        count += 1;
        if count > MAX_PACKAGE_FILES {
            bail!("package archive has more than {MAX_PACKAGE_FILES} entries");
        }
        if top.is_none() {
            if let Some(Component::Normal(first)) = path.components().next() {
                top = Some(into.join(first));
            }
        }
        entry.unpack_in(into)?;
    }
    let top = top.context("empty package archive")?;
    if !top.join(MANIFEST_FILE).is_file() {
        bail!("archive has no {MANIFEST_FILE} at its root");
    }
    Ok(top)
}

// ── 装 / 卸 ──

/// 一个待写入的文件:包里的源、安装后的绝对路径。
#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub executable: bool,
}

#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub manifest: PackageManifest,
    pub files: Vec<PlannedFile>,
    pub persona_scope: Option<String>,
}

impl InstallPlan {
    pub fn summary_lines(&self, root: &Path) -> Vec<String> {
        self.files
            .iter()
            .map(|file| {
                file.destination
                    .strip_prefix(root)
                    .unwrap_or(&file.destination)
                    .display()
                    .to_string()
            })
            .collect()
    }
}

fn relative_within(root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside {}", path.display(), root.display()))?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("unsupported path component in {}", relative.display());
    }
    Ok(relative.to_path_buf())
}

/// 很小的 glob:只支持路径末段的 `*`(`scripts/*`、`skills/*`、`tools/*.py`)。
fn expand_glob(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let pattern = pattern.trim().trim_start_matches("./");
    if pattern.is_empty() || pattern.starts_with('/') || pattern.contains("..") {
        bail!("invalid install pattern {pattern:?}");
    }
    let (dir, leaf) = match pattern.rsplit_once('/') {
        Some((dir, leaf)) => (root.join(dir), leaf),
        None => (root.to_path_buf(), pattern),
    };
    if dir.components().count() > 0 && !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut matches = Vec::new();
    if let Some(star) = leaf.find('*') {
        let (prefix, suffix) = (&leaf[..star], &leaf[star + 1..]);
        if suffix.contains('*') {
            bail!("only one '*' is supported in {pattern:?}");
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !name.starts_with(prefix) || !name.ends_with(suffix) {
                continue;
            }
            matches.push(entry.path());
        }
    } else {
        let path = dir.join(leaf);
        if path.exists() {
            matches.push(path);
        }
    }
    matches.sort();
    Ok(matches)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if file_type.is_dir() {
            collect_files(&entry.path(), out)?;
        } else {
            out.push(entry.path());
            if out.len() > MAX_PACKAGE_FILES {
                bail!("package has more than {MAX_PACKAGE_FILES} files");
            }
        }
    }
    Ok(())
}

/// 把清单摊成文件清单。不写盘。
pub fn plan_install(
    config: &crate::config::AppConfig,
    paths: &MiyuPaths,
    package_root: &Path,
) -> Result<InstallPlan> {
    let manifest = PackageManifest::load(package_root)?;
    manifest.check_requirement()?;
    let name = manifest.package.name.clone();
    let mut files = Vec::new();
    let persona_scope = match manifest.package.kind {
        PackageKind::Persona => Some(persona_scope_name(&format!("{name}.md"))),
        PackageKind::Extension => None,
    };
    let scripts_root = match &persona_scope {
        Some(scope) => paths.scripts_dir.join("personas").join(scope),
        None => paths.scripts_dir.clone(),
    };
    let skills_root = match &persona_scope {
        Some(scope) => paths.skills_dir.join("personas").join(scope),
        None => paths.skills_dir.clone(),
    };
    let script_patterns = manifest
        .install
        .scripts
        .clone()
        .unwrap_or_else(|| vec!["scripts/*".to_string()]);
    for pattern in &script_patterns {
        for source in expand_glob(package_root, pattern)? {
            if !source.is_file() {
                continue;
            }
            let file_name = source
                .file_name()
                .context("script without a file name")?
                .to_owned();
            files.push(PlannedFile {
                destination: scripts_root.join(file_name),
                source,
                executable: true,
            });
        }
    }
    let skill_patterns = manifest
        .install
        .skills
        .clone()
        .unwrap_or_else(|| vec!["skills/*".to_string()]);
    for pattern in &skill_patterns {
        for skill_dir in expand_glob(package_root, pattern)? {
            if !skill_dir.is_dir() {
                continue;
            }
            let skill_file = skill_dir.join("SKILL.md");
            if !skill_file.is_file() {
                bail!("skill directory {} has no SKILL.md", skill_dir.display());
            }
            let dir_name = skill_dir
                .file_name()
                .and_then(|value| value.to_str())
                .context("skill directory name is not UTF-8")?
                .to_string();
            let raw = fs::read_to_string(&skill_file)?;
            crate::skills::manifest::parse_skill_metadata(&raw, Some(&dir_name))
                .with_context(|| format!("invalid skill {}", skill_dir.display()))?;
            let mut skill_files = Vec::new();
            collect_files(&skill_dir, &mut skill_files)?;
            for source in skill_files {
                let relative = relative_within(&skill_dir, &source)?;
                files.push(PlannedFile {
                    destination: skills_root.join(&dir_name).join(relative),
                    source,
                    executable: false,
                });
            }
        }
    }
    if let Some(scope) = &persona_scope {
        let persona_dir = package_root.join(
            manifest
                .install
                .persona
                .clone()
                .unwrap_or_else(|| "persona".to_string()),
        );
        let prompt = persona_dir.join("persona.md");
        if !prompt.is_file() {
            bail!("persona package {} has no {}", name, prompt.display());
        }
        let prompts_dir = config.prompts_dir_path(paths);
        files.push(PlannedFile {
            destination: prompts_dir.join(format!("{name}.md")),
            source: prompt,
            executable: false,
        });
        let meta = persona_dir.join("persona.json");
        if meta.is_file() {
            files.push(PlannedFile {
                destination: prompts_dir.join(format!("{name}.json")),
                source: meta,
                executable: false,
            });
        }
        let persona_toml = persona_dir.join("persona.toml");
        if persona_toml.is_file() {
            let raw = fs::read_to_string(&persona_toml)?;
            crate::config::PersonaManifest::parse(&raw)
                .with_context(|| format!("invalid {}", persona_toml.display()))?;
            files.push(PlannedFile {
                destination: paths.personas_dir().join(scope).join("persona.toml"),
                source: persona_toml,
                executable: false,
            });
        }
        let assets = persona_dir.join("assets");
        if assets.is_dir() {
            let mut asset_files = Vec::new();
            collect_files(&assets, &mut asset_files)?;
            for source in asset_files {
                let relative = relative_within(&assets, &source)?;
                files.push(PlannedFile {
                    destination: paths.persona_avatars_dir().join(&name).join(relative),
                    source,
                    executable: false,
                });
            }
        }
    }
    if files.is_empty() {
        bail!("package {name} installs nothing (no scripts, skills or persona matched)");
    }
    // 目标不能出 MIYU_HOME
    for file in &files {
        relative_within(&paths.root_dir, &file.destination)?;
    }
    Ok(InstallPlan {
        manifest,
        files,
        persona_scope,
    })
}

fn fingerprint_files(files: &[PlannedFile]) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut sorted: Vec<&PlannedFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.destination.cmp(&b.destination));
    for file in sorted {
        hasher.update(file.destination.as_os_str().as_encoded_bytes());
        hasher.update(&[0]);
        hasher.update(&fs::read(&file.source)?);
        hasher.update(&[0]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// 装:先查冲突(目标已存在且不是本包的),再逐个复制,最后记锁。
/// 复制中途失败会把已写的删掉。
pub fn install(
    paths: &MiyuPaths,
    plan: &InstallPlan,
    source: &PackageSource,
    commit: Option<String>,
    force: bool,
) -> Result<InstalledPackage> {
    let name = &plan.manifest.package.name;
    let mut lock = load_lock(paths)?;
    let owned_before: Vec<PathBuf> = lock
        .packages
        .get(name)
        .map(|installed| {
            installed
                .files
                .iter()
                .map(|relative| paths.root_dir.join(relative))
                .collect()
        })
        .unwrap_or_default();
    for file in &plan.files {
        if file.destination.exists() && !owned_before.contains(&file.destination) && !force {
            let owner = lock.packages.iter().find_map(|(other, installed)| {
                installed
                    .files
                    .iter()
                    .any(|relative| paths.root_dir.join(relative) == file.destination)
                    .then_some(other.clone())
            });
            match owner {
                Some(other) => bail!(
                    "{} is owned by package {other}; remove it first",
                    file.destination.display()
                ),
                None => bail!(
                    "{} already exists and was not installed by miyu pm (use --force to overwrite)",
                    file.destination.display()
                ),
            }
        }
    }
    let fingerprint = fingerprint_files(&plan.files)?;
    // 先卸旧文件(升级/重装),再写新的
    if let Some(previous) = lock.packages.remove(name) {
        remove_files(paths, &previous.files)?;
    }
    let mut written = Vec::new();
    let result = (|| -> Result<()> {
        for file in &plan.files {
            if let Some(parent) = file.destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&file.source, &file.destination).with_context(|| {
                format!(
                    "copying {} to {}",
                    file.source.display(),
                    file.destination.display()
                )
            })?;
            if file.executable {
                fs::set_permissions(&file.destination, fs::Permissions::from_mode(0o755))?;
            }
            written.push(file.destination.clone());
        }
        Ok(())
    })();
    if let Err(error) = result {
        for path in &written {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    let installed = InstalledPackage {
        source: source.describe(),
        reference: source.reference(),
        commit,
        version: plan.manifest.package.version.clone(),
        kind: plan.manifest.package.kind,
        description: plan.manifest.package.description.clone(),
        installed_at: chrono::Utc::now().to_rfc3339(),
        files: plan
            .files
            .iter()
            .map(|file| {
                relative_within(&paths.root_dir, &file.destination)
                    .map(|relative| relative.display().to_string())
            })
            .collect::<Result<Vec<_>>>()?,
        fingerprint,
    };
    lock.packages.insert(name.clone(), installed.clone());
    save_lock(paths, &lock)?;
    Ok(installed)
}

fn remove_files(paths: &MiyuPaths, files: &[String]) -> Result<()> {
    let mut dirs = std::collections::BTreeSet::new();
    for relative in files {
        let path = paths.root_dir.join(relative);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("removing {}", path.display()))
            }
        }
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if dir == paths.root_dir
                || dir == paths.scripts_dir
                || dir == paths.skills_dir
                || dir == paths.data_dir
            {
                break;
            }
            dirs.insert(dir.to_path_buf());
            parent = dir.parent();
        }
    }
    // 深的先删:只删空目录
    for dir in dirs.iter().rev() {
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

pub fn remove(paths: &MiyuPaths, name: &str) -> Result<InstalledPackage> {
    let mut lock = load_lock(paths)?;
    let installed = lock
        .packages
        .remove(name)
        .with_context(|| format!("package {name:?} is not installed"))?;
    remove_files(paths, &installed.files)?;
    save_lock(paths, &lock)?;
    Ok(installed)
}

/// 升级前的判断:来源相同、commit 相同(都拿得到)或指纹相同 → 不用动。
pub fn is_up_to_date(
    installed: &InstalledPackage,
    plan: &InstallPlan,
    commit: Option<&str>,
) -> Result<bool> {
    if let (Some(old), Some(new)) = (installed.commit.as_deref(), commit) {
        if old == new {
            return Ok(true);
        }
    }
    Ok(fingerprint_files(&plan.files)? == installed.fingerprint)
}

#[cfg(test)]
mod tests;
