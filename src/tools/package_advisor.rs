use super::{ToolRegistry, ToolSpec};
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const AUR_REVIEW_RULES: &str = include_str!("../prompts/aur-review.md");
const MAX_FILE_CHARS: usize = 24_000;
const MAX_FILES: usize = 80;
const FETCH_TIMEOUT_SECONDS: u64 = 120;
const INSTALL_TIMEOUT_SECONDS: u64 = 900;
const MAKEPKG_TIMEOUT_SECONDS: u64 = 1800;

pub fn register(registry: &mut ToolRegistry, paths: MiyuPaths) {
    let review_paths = paths.clone();
    registry.register(ToolSpec::new(
        "review_aur_package",
        "Fetch AUR build files and prepare a PKGBUILD security review. After review, stop and ask the user whether to install; do not call install_aur_package in the same turn.",
        json!({"type":"object","properties":{"package":{"type":"string","description":"AUR package name."}},"required":["package"],"additionalProperties":false}),
        move |args| {
            let paths = review_paths.clone();
            async move { review_aur_package(args, paths).await }
        },
    ));
    let install_paths = paths.clone();
    registry.register(ToolSpec::new(
        "install_aur_package",
        "Install an AUR package only after review_aur_package recorded an allowed review state and the user explicitly confirmed installation in a later reply. Requires user_confirmed=true. Tries paru, then yay, then AUR snapshot + makepkg + pacman -U fallback.",
        json!({"type":"object","properties":{"package":{"type":"string","description":"AUR package name."},"user_confirmed":{"type":"boolean","description":"Set true only when the user explicitly confirmed installation after seeing the review."}},"required":["package","user_confirmed"],"additionalProperties":false}),
        move |args| {
            let paths = install_paths.clone();
            async move { install_aur_package(args, paths).await }
        },
    ).writes());
}

async fn review_aur_package(args: Value, paths: MiyuPaths) -> Result<String> {
    let package = required(&args, "package")?;
    validate_package_name(&package)?;
    let metadata = fetch_aur_metadata(&package).await?;
    let root = paths.cache_dir.join("aur-review").join(&package);
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::create_dir_all(&root)?;
    let fetched_by = if let Some(helper) = aur_helper().await {
        fetch_with_helper(&helper, &package, &root).await?;
        helper
    } else {
        fetch_with_curl_fallback(&metadata, &root).await?;
        "curl-fallback".to_string()
    };
    let build_dir = find_pkgbuild_dir(&root)?;
    let files = review_files(&build_dir)?;
    let risk = heuristic_risk(&files);
    let install_allowed = risk["level"] != "high";
    record_review_state(&paths, &package, &risk, install_allowed)?;
    review_result(
        &build_dir,
        Some(package),
        Some(metadata),
        Some(fetched_by),
        files,
        Some(risk),
        Some(install_allowed),
    )
}

async fn install_aur_package(args: Value, paths: MiyuPaths) -> Result<String> {
    let package = required(&args, "package")?;
    if args.get("user_confirmed").and_then(Value::as_bool) != Some(true) {
        bail!("AUR install requires explicit user confirmation after review: {package}")
    }
    validate_package_name(&package)?;
    let review = review_state_for_package(&paths, &package)?
        .ok_or_else(|| anyhow::anyhow!("AUR package must be reviewed before install: {package}"))?;
    if !review["install_allowed"].as_bool().unwrap_or(false) {
        bail!("AUR package review did not allow install: {package}")
    }
    record_install_confirmation(&paths, &package)?;
    let review = review_state_for_package(&paths, &package)?
        .ok_or_else(|| anyhow::anyhow!("AUR package must be reviewed before install: {package}"))?;
    let result = if let Some(helper) = aur_helper().await {
        install_with_helper(&helper, &package).await?
    } else {
        install_with_makepkg_fallback(&package, &paths).await?
    };
    Ok(serde_json::to_string_pretty(&json!({
        "ok": result["ok"].as_bool().unwrap_or(false),
        "package": package,
        "review": review,
        "install_result": result,
        "output_instruction": "Explain that install was allowed because review_aur_package recorded an allowed review state and the user explicitly confirmed installation. Include install success or failure concisely."
    }))?)
}

fn review_result(
    build_dir: &Path,
    package: Option<String>,
    metadata: Option<Value>,
    fetched_by: Option<String>,
    files: Vec<Value>,
    risk: Option<Value>,
    install_allowed: Option<bool>,
) -> Result<String> {
    if !build_dir.join("PKGBUILD").is_file() {
        bail!("PKGBUILD not found in {}", build_dir.display());
    }
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "package": package,
        "build_dir": build_dir.display().to_string(),
        "fetched_by": fetched_by,
        "aur_metadata": metadata,
        "risk": risk,
        "install_allowed": install_allowed,
        "files_reviewed": files.iter().map(|file| &file["path"]).collect::<Vec<_>>(),
        "files": files,
        "review_rules": AUR_REVIEW_RULES,
        "output_instruction": "Use review_rules exactly, but omit the PAC_DECISION machine-readable line in the final answer. Mention risk.level and install_allowed. Do not install, build, run makepkg, or ask follow-up questions unless required files are missing. If install_allowed is true, ask the user whether to install and stop."
    }))?)
}

async fn fetch_aur_metadata(package: &str) -> Result<Value> {
    let url = format!("https://aur.archlinux.org/rpc/v5/info?arg[]={package}");
    let value: Value = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if value["resultcount"].as_u64().unwrap_or(0) == 0 {
        bail!("AUR package not found: {package}");
    }
    Ok(value["results"][0].clone())
}

async fn aur_helper() -> Option<String> {
    for helper in ["paru", "yay"] {
        if Command::new("sh")
            .arg("-lc")
            .arg(format!("command -v {helper}"))
            .stdin(Stdio::null())
            .output()
            .await
            .ok()
            .is_some_and(|output| output.status.success())
        {
            return Some(helper.to_string());
        }
    }
    None
}

async fn fetch_with_helper(helper: &str, package: &str, root: &Path) -> Result<()> {
    let output = Command::new(helper)
        .arg("--aur")
        .arg("--redownload")
        .arg("-G")
        .arg(package)
        .current_dir(root)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = command_output_with_timeout(output, helper, FETCH_TIMEOUT_SECONDS).await?;
    if !output.status.success() {
        bail!(
            "{helper} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn fetch_with_curl_fallback(metadata: &Value, root: &Path) -> Result<()> {
    let url_path = metadata["URLPath"]
        .as_str()
        .context("AUR metadata missing URLPath")?;
    let url = format!("https://aur.archlinux.org{url_path}");
    let bytes = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let decoder = GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(root)?;
    Ok(())
}

async fn install_with_helper(helper: &str, package: &str) -> Result<Value> {
    let output = Command::new(helper)
        .arg("-S")
        .arg("--noconfirm")
        .arg("--needed")
        .arg(package)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = command_output_with_timeout(output, helper, INSTALL_TIMEOUT_SECONDS).await?;
    Ok(command_result(helper, output))
}

async fn install_with_makepkg_fallback(package: &str, paths: &MiyuPaths) -> Result<Value> {
    let metadata = fetch_aur_metadata(package).await?;
    let root = paths.cache_dir.join("aur-install").join(package);
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::create_dir_all(&root)?;
    fetch_with_curl_fallback(&metadata, &root).await?;
    let build_dir = find_pkgbuild_dir(&root)?;
    let makepkg = Command::new("makepkg")
        .arg("--noconfirm")
        .current_dir(&build_dir)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let makepkg = command_output_with_timeout(makepkg, "makepkg", MAKEPKG_TIMEOUT_SECONDS).await?;
    if !makepkg.status.success() {
        return Ok(command_result("makepkg", makepkg));
    }
    let package_file = find_built_package(&build_dir)?;
    let pacman = Command::new("pacman")
        .arg("-U")
        .arg("--noconfirm")
        .arg(&package_file)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let pacman = command_output_with_timeout(pacman, "pacman -U", INSTALL_TIMEOUT_SECONDS).await?;
    Ok(command_result("pacman -U", pacman))
}

async fn command_output_with_timeout(
    output: impl std::future::Future<Output = std::io::Result<std::process::Output>>,
    command: &str,
    timeout_seconds: u64,
) -> Result<std::process::Output> {
    match timeout(Duration::from_secs(timeout_seconds), output).await {
        Ok(output) => Ok(output?),
        Err(_) => bail!("{command} timed out after {timeout_seconds}s"),
    }
}

fn command_result(command: &str, output: std::process::Output) -> Value {
    json!({
        "ok": output.status.success(),
        "command": command,
        "exit_code": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout).trim(),
        "stderr": String::from_utf8_lossy(&output.stderr).trim(),
    })
}

fn find_built_package(build_dir: &Path) -> Result<PathBuf> {
    let mut packages = Vec::new();
    for entry in std::fs::read_dir(build_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.contains(".pkg.tar") {
            packages.push(path);
        }
    }
    packages
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("makepkg did not produce a package archive"))
}

fn find_pkgbuild_dir(root: &Path) -> Result<PathBuf> {
    if root.join("PKGBUILD").is_file() {
        return Ok(root.to_path_buf());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.path().join("PKGBUILD").is_file() {
            return Ok(entry.path());
        }
    }
    bail!("PKGBUILD not found after fetching AUR snapshot")
}

fn review_files(build_dir: &Path) -> Result<Vec<Value>> {
    let mut files = Vec::new();
    collect_file(build_dir, Path::new("PKGBUILD"), &mut files)?;
    collect_file(build_dir, Path::new(".SRCINFO"), &mut files).ok();
    for entry in walk_limited(build_dir, 2)? {
        if files.len() >= MAX_FILES {
            break;
        }
        let rel = entry.strip_prefix(build_dir).unwrap_or(&entry);
        if should_review_extra_file(rel) {
            collect_file(build_dir, rel, &mut files).ok();
        }
    }
    Ok(files)
}

fn collect_file(build_dir: &Path, rel: &Path, files: &mut Vec<Value>) -> Result<()> {
    if files
        .iter()
        .any(|file| file["path"] == rel.display().to_string())
    {
        return Ok(());
    }
    let path = build_dir.join(rel);
    if !path.is_file() {
        return Ok(());
    }
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|_| "<non-utf8 file omitted>".to_string());
    let truncated = content.chars().count() > MAX_FILE_CHARS;
    let content = if truncated {
        content.chars().take(MAX_FILE_CHARS).collect::<String>()
    } else {
        content
    };
    files.push(
        json!({"path": rel.display().to_string(), "truncated": truncated, "content": content}),
    );
    Ok(())
}

fn walk_limited(root: &Path, depth: usize) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
        if depth == 0 {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk(&path, depth - 1, out).ok();
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    walk(root, depth, &mut out)?;
    out.sort();
    Ok(out)
}

fn should_review_extra_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default(),
        "install" | "patch" | "diff" | "sh" | "service" | "timer" | "socket" | "desktop"
    ) || name.ends_with(".install")
        || name.contains("sysusers")
        || name.contains("tmpfiles")
}

fn heuristic_risk(files: &[Value]) -> Value {
    let mut findings = Vec::new();
    for file in files {
        let path = file["path"].as_str().unwrap_or_default();
        let content = file["content"].as_str().unwrap_or_default();
        let lower = content.to_ascii_lowercase();
        for pattern in [
            "curl ",
            "wget ",
            "| sh",
            "|sh",
            "chmod 777",
            "chown root",
            "setcap ",
            "systemctl enable",
            "rm -rf /",
            "skipsums",
            "sha256sums=('skip'",
            "sha256sums=(\"skip\"",
        ] {
            if lower.contains(pattern) {
                findings.push(json!({"file": path, "pattern": pattern}));
            }
        }
    }
    let level = if findings.iter().any(|finding| {
        finding["pattern"] == "| sh"
            || finding["pattern"] == "|sh"
            || finding["pattern"] == "rm -rf /"
    }) {
        "high"
    } else if findings.is_empty() {
        "low"
    } else {
        "medium"
    };
    json!({"level": level, "findings": findings})
}

pub fn clear_aur_review_state(paths: &MiyuPaths) -> Result<()> {
    let path = aur_review_state_path(paths);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn record_review_state(
    paths: &MiyuPaths,
    package: &str,
    risk: &Value,
    install_allowed: bool,
) -> Result<()> {
    std::fs::create_dir_all(&paths.state_dir)?;
    let mut state = load_review_state(paths)?;
    state[package] = json!({
        "package": package,
        "reviewed_at_unix": current_unix_seconds(),
        "risk": risk,
        "install_allowed": install_allowed,
        "user_confirmed_install": false,
    });
    std::fs::write(
        aur_review_state_path(paths),
        format!("{}\n", serde_json::to_string_pretty(&state)?),
    )?;
    Ok(())
}

fn review_state_for_package(paths: &MiyuPaths, package: &str) -> Result<Option<Value>> {
    Ok(load_review_state(paths)?.get(package).cloned())
}

fn record_install_confirmation(paths: &MiyuPaths, package: &str) -> Result<()> {
    let mut state = load_review_state(paths)?;
    let Some(entry) = state.get_mut(package) else {
        bail!("AUR package must be reviewed before install: {package}")
    };
    entry["user_confirmed_install"] = json!(true);
    entry["user_confirmed_at_unix"] = json!(current_unix_seconds());
    std::fs::write(
        aur_review_state_path(paths),
        format!("{}\n", serde_json::to_string_pretty(&state)?),
    )?;
    Ok(())
}

fn load_review_state(paths: &MiyuPaths) -> Result<Value> {
    let path = aur_review_state_path(paths);
    if !path.exists() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

fn aur_review_state_path(paths: &MiyuPaths) -> PathBuf {
    paths.state_dir.join("aur-review-state.json")
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn validate_package_name(package: &str) -> Result<()> {
    if package.is_empty()
        || !package
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '+' | '.'))
    {
        bail!("invalid package name: {package}");
    }
    Ok(())
}

fn required(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() {
        bail!("missing required argument: {key}")
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_aur_package_names() {
        assert!(validate_package_name("paru").is_ok());
        assert!(validate_package_name("foo-bar_git+1.0").is_ok());
        assert!(validate_package_name("foo;rm -rf /").is_err());
    }

    #[test]
    fn selects_extra_review_files() {
        assert!(should_review_extra_file(Path::new("foo.install")));
        assert!(should_review_extra_file(Path::new("app.service")));
        assert!(should_review_extra_file(Path::new("fix.patch")));
        assert!(!should_review_extra_file(Path::new("README.md")));
    }

    #[test]
    fn heuristic_risk_blocks_pipe_to_shell() {
        let files =
            vec![json!({"path":"PKGBUILD", "content":"curl https://example.test/install.sh | sh"})];
        let risk = heuristic_risk(&files);
        assert_eq!(risk["level"], "high");
    }

    #[test]
    fn review_state_records_and_clears_package() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path().to_path_buf());
        let risk = json!({"level":"low", "findings": []});
        record_review_state(&paths, "foo", &risk, true).unwrap();
        let state = review_state_for_package(&paths, "foo").unwrap().unwrap();
        assert_eq!(state["install_allowed"], true);
        assert_eq!(state["user_confirmed_install"], false);
        record_install_confirmation(&paths, "foo").unwrap();
        let state = review_state_for_package(&paths, "foo").unwrap().unwrap();
        assert_eq!(state["user_confirmed_install"], true);
        clear_aur_review_state(&paths).unwrap();
        assert!(review_state_for_package(&paths, "foo").unwrap().is_none());
    }

    #[test]
    fn install_confirmation_requires_existing_review() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path().to_path_buf());
        assert!(record_install_confirmation(&paths, "foo").is_err());
    }

    fn test_paths(state_dir: PathBuf) -> MiyuPaths {
        MiyuPaths {
            config_dir: PathBuf::new(),
            config_file: PathBuf::new(),
            secrets_file: PathBuf::new(),
            skills_dir: PathBuf::new(),
            data_dir: PathBuf::new(),
            cache_dir: state_dir.join("cache"),
            state_dir,
            pictures_dir: PathBuf::new(),
            fish_hook_file: PathBuf::new(),
            bash_hook_file: PathBuf::new(),
            zsh_hook_file: PathBuf::new(),
            scripts_dir: PathBuf::new(),
            system_scripts_dir: PathBuf::new(),
        }
    }
}
