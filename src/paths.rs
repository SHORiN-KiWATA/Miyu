use crate::i18n::text as t;
use anyhow::{bail, Context, Result};
use directories::{BaseDirs, UserDirs};
use serde::{Deserialize, Serialize};
use crate::sys;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file as symlink;

const LAYOUT_MARKER: &str = ".layout-v1";
const RESOURCE_LAYOUT_MARKER: &str = ".resource-layout-v1";
const RESOURCE_MIGRATION_JOURNAL: &str = ".resource-layout-v1.journal.json";

#[derive(Debug, Clone)]
pub struct MiyuPaths {
    /// Everything below lives under this root (`~/.miyu`, or `MIYU_HOME`).
    /// Kept as its own field because the model is told where Miyu's files are
    /// and guessing it back from a child directory would silently break the
    /// day the layout changes.
    pub root_dir: PathBuf,
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub skills_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub pictures_dir: PathBuf,
    pub fish_hook_file: PathBuf,
    pub bash_hook_file: PathBuf,
    pub zsh_hook_file: PathBuf,
    pub scripts_dir: PathBuf,
    pub system_scripts_dir: PathBuf,
}

impl MiyuPaths {
    pub fn new() -> Result<Self> {
        let base = BaseDirs::new().context(t(
            "could not determine XDG base directories",
            "无法确定 XDG 基础目录",
        ))?;
        let legacy_config_dir = base.config_dir().join("miyu");
        let legacy_data_dir = base.data_dir().join("miyu");
        let legacy_cache_dir = base.cache_dir().join("miyu");
        let legacy_state_dir = base
            .state_dir()
            .unwrap_or_else(|| base.data_dir())
            .join("miyu");
        let legacy_documents_dir = UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(PathBuf::from))
            .unwrap_or_else(|| base.home_dir().join("Documents"))
            .join("Miyu");
        let legacy_pictures_root = std::env::var_os("XDG_PICTURES_DIR")
            .map(PathBuf::from)
            .or_else(|| UserDirs::new().and_then(|dirs| dirs.picture_dir().map(PathBuf::from)))
            .unwrap_or_else(|| base.home_dir().join("Pictures"));
        let explicit_home = std::env::var_os("MIYU_HOME").map(PathBuf::from);
        let root_dir = explicit_home
            .clone()
            .unwrap_or_else(|| base.home_dir().join(".miyu"));
        let config_dir = root_dir.join("config");
        let data_dir = root_dir.join("data");
        let cache_dir = root_dir.join("cache");
        let state_dir = root_dir.join("state");

        let legacy = LegacyLayout {
            config_dir: legacy_config_dir.clone(),
            data_dir: legacy_data_dir.clone(),
            cache_dir: legacy_cache_dir.clone(),
            state_dir: legacy_state_dir.clone(),
            documents_dir: legacy_documents_dir,
            pictures_dirs: vec![
                legacy_pictures_root.join("miyu"),
                legacy_pictures_root.join("Miyu"),
            ],
        };
        let next = Layout {
            root_dir: root_dir.clone(),
            config_dir: config_dir.clone(),
            data_dir: data_dir.clone(),
            cache_dir: cache_dir.clone(),
            state_dir: state_dir.clone(),
        };

        // A client from a newly installed binary may start while the previous
        // daemon still has the legacy SQLite files open. Keep that client on
        // the legacy layout; daemon version negotiation will stop the old
        // process, and the newly spawned daemon performs the migration.
        let migration_enabled = explicit_home.is_none() && !cfg!(test);
        let marker_exists = layout_marker_exists(&next)?;
        let legacy_exists = legacy.exists()?;
        let use_legacy_temporarily = migration_enabled
            && !marker_exists
            && legacy_exists
            && legacy_daemon_is_running(&legacy);
        let (config_dir, data_dir, cache_dir, state_dir) = if use_legacy_temporarily {
            (
                legacy_config_dir,
                legacy_data_dir,
                legacy_cache_dir,
                legacy_state_dir,
            )
        } else {
            if migration_enabled {
                migrate_legacy_layout(&legacy, &next)?;
            } else if explicit_home.is_some() {
                ensure_private_dir(&root_dir)?;
            }
            (config_dir, data_dir, cache_dir, state_dir)
        };
        let resource_layout = Layout {
            root_dir: root_dir.clone(),
            config_dir: config_dir.clone(),
            data_dir: data_dir.clone(),
            cache_dir: cache_dir.clone(),
            state_dir: state_dir.clone(),
        };
        let resource_marker_exists = resource_layout_marker_exists(&resource_layout)?;
        let daemon_process = current_process_is_daemon();
        let resource_migration_deferred =
            if use_legacy_temporarily || resource_marker_exists || cfg!(test) {
                false
            } else {
                !try_migrate_resource_layout(&resource_layout, daemon_process)?
            };
        let pictures_dir = if use_legacy_temporarily {
            legacy_pictures_root.join("miyu")
        } else {
            data_dir.join("pictures")
        };
        let fish_hook_file = base.config_dir().join("fish/conf.d/miyu.fish");
        let bash_hook_file = config_dir.join("shell/bash-hook.sh");
        let zsh_hook_file = config_dir.join("shell/zsh-hook.zsh");
        let resource_config_dir = if use_legacy_temporarily || resource_migration_deferred {
            config_dir.clone()
        } else {
            data_dir.clone()
        };
        let scripts_dir = resource_config_dir.join("scripts");
        let system_scripts_dir = PathBuf::from("/usr/share/miyu/scripts");

        Ok(Self {
            // The canonical home even inside the transient legacy window: that
            // window only exists while an old daemon still holds the XDG files
            // open, and it closes as soon as the new daemon migrates them here.
            root_dir,
            config_file: config_dir.join("config.jsonc"),
            skills_dir: resource_config_dir.join("skills"),
            config_dir,
            data_dir,
            cache_dir,
            state_dir,
            pictures_dir,
            fish_hook_file,
            bash_hook_file,
            zsh_hook_file,
            scripts_dir,
            system_scripts_dir,
        })
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root_dir.join("bin")
    }

    pub fn ensure_bin_installed(&self) -> Result<bool> {
        let bin_dir = self.bin_dir();
        ensure_private_dir(&bin_dir)?;
        if let Ok(current_exe) = std::env::current_exe() {
            let target_exe_name = if cfg!(windows) { "miyu.exe" } else { "miyu" };
            let target_exe = bin_dir.join(target_exe_name);
            if current_exe != target_exe {
                let _ = fs::copy(&current_exe, &target_exe);
            }
            if let Some(parent) = current_exe.parent() {
                let _ = sys::install_windows_user_path(parent);
            }
        }
        Ok(sys::install_windows_user_path(&bin_dir)?)
    }

    pub fn create_dirs(&self) -> Result<()> {
        let prompts_dir = self.prompts_dir();
        let identities_dir = self.identities_dir();
        let persona_avatars_dir = self.persona_avatars_dir();
        let skill_drafts_dir = self.skill_drafts_dir();
        let bin_dir = self.bin_dir();
        for directory in [
            &self.config_dir,
            &self.skills_dir,
            &self.data_dir,
            &self.cache_dir,
            &self.state_dir,
            &self.pictures_dir,
            &self.scripts_dir,
            &bin_dir,
            &prompts_dir,
            &identities_dir,
            &persona_avatars_dir,
            &skill_drafts_dir,
        ] {
            ensure_private_dir(directory)?;
        }
        Ok(())
    }

    /// Returns the root used for Miyu-owned, user-authored resources. During
    /// an upgrade this intentionally remains the old config directory until
    /// the resource migration marker has been committed.
    pub fn resource_dir(&self) -> &Path {
        if self.skills_dir == self.data_dir.join("skills") {
            &self.data_dir
        } else {
            &self.config_dir
        }
    }

    pub fn resources_use_config_dir(&self) -> bool {
        self.resource_dir() == self.config_dir
    }

    pub fn legacy_config_dir(&self) -> Option<PathBuf> {
        let base = BaseDirs::new()?;
        (self.config_dir == base.home_dir().join(".miyu/config"))
            .then(|| base.config_dir().join("miyu"))
    }

    pub fn migrated_resource_path(&self, path: &Path) -> Option<PathBuf> {
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.config_dir).ok().or_else(|| {
                self.legacy_config_dir()
                    .as_deref()
                    .and_then(|legacy| path.strip_prefix(legacy).ok())
            })?
        } else {
            path
        };
        let relative = normalize_resource_relative_path(relative)?;
        let namespace = relative.components().next()?.as_os_str().to_str()?;
        if !matches!(
            namespace,
            "skills" | "scripts" | "prompts" | "identities" | "persona-avatars"
        ) {
            return None;
        }
        Some(self.resource_dir().join(relative))
    }

    pub fn prompts_dir(&self) -> PathBuf {
        self.resource_dir().join("prompts")
    }

    pub fn identities_dir(&self) -> PathBuf {
        self.resource_dir().join("identities")
    }

    pub fn persona_avatars_dir(&self) -> PathBuf {
        self.resource_dir().join("persona-avatars")
    }

    pub fn skill_drafts_dir(&self) -> PathBuf {
        self.state_dir.join("skill-drafts")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.cache_dir.join("logs")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(runtime_dir) => runtime_dir_for(
                Path::new(&runtime_dir),
                std::env::var_os("MIYU_HOME").as_deref().map(Path::new),
            ),
            None => self.state_dir.join("miyu"),
        }
    }

    pub fn ipc_socket(&self) -> PathBuf {
        self.runtime_dir().join("core.sock")
    }

    pub fn ipc_lock(&self) -> PathBuf {
        self.runtime_dir().join("core.lock")
    }

    pub fn daemon_start_lock(&self) -> PathBuf {
        self.runtime_dir().join("starter.lock")
    }

    pub fn daemon_launch_state_file(&self) -> PathBuf {
        self.state_dir.join("daemon-launch.json")
    }

    pub fn managed_web_password_dir(&self) -> PathBuf {
        self.state_dir.join("web-passwords")
    }

    pub fn print(&self) {
        println!(
            "{}: {}",
            t("config directory", "配置目录"),
            self.config_dir.display()
        );
        println!(
            "{}: {}",
            t("config file", "配置文件"),
            self.config_file.display()
        );
        println!(
            "{}: {}",
            t("skills directory", "skills 目录"),
            self.skills_dir.display()
        );
        println!(
            "{}: {}",
            t("skill drafts directory", "skill 草稿目录"),
            self.skill_drafts_dir().display()
        );
        println!(
            "{}: {}",
            t("prompts directory", "prompts 目录"),
            self.prompts_dir().display()
        );
        println!(
            "{}: {}",
            t("identities directory", "identities 目录"),
            self.identities_dir().display()
        );
        println!(
            "{}: {}",
            t("persona avatars directory", "人格头像目录"),
            self.persona_avatars_dir().display()
        );
        println!(
            "{}: {}",
            t("data directory", "数据目录"),
            self.data_dir.display()
        );
        println!(
            "{}: {}",
            t("cache directory", "缓存目录"),
            self.cache_dir.display()
        );
        println!(
            "{}: {}",
            t("state directory", "状态目录"),
            self.state_dir.display()
        );
        println!(
            "{}: {}",
            t("log directory", "日志目录"),
            self.logs_dir().display()
        );
        println!(
            "{}: {}",
            t("pictures directory", "图片目录"),
            self.pictures_dir.display()
        );
        println!(
            "{}: {}",
            t("fish hook file", "fish hook 文件"),
            self.fish_hook_file.display()
        );
        println!(
            "{}: {}",
            t("bash hook file", "bash hook 文件"),
            self.bash_hook_file.display()
        );
        println!(
            "{}: {}",
            t("zsh hook file", "zsh hook 文件"),
            self.zsh_hook_file.display()
        );
        println!(
            "{}: {}",
            t("scripts directory", "scripts 目录"),
            self.scripts_dir.display()
        );
        println!(
            "{}: {}",
            t("system scripts directory", "系统 scripts 目录"),
            self.system_scripts_dir.display()
        );
    }
}

fn runtime_dir_for(runtime_root: &Path, explicit_home: Option<&Path>) -> PathBuf {
    let name = explicit_home.map_or_else(
        || "miyu".to_string(),
        |home| {
            let normalized = normalize_home(home);
            let digest = blake3::hash(normalized.as_os_str().as_encoded_bytes());
            format!("miyu-{}", &digest.to_hex()[..12])
        },
    );
    runtime_root.join(name)
}

fn normalize_home(home: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(home) {
        return canonical;
    }
    let absolute = if home.is_absolute() {
        home.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(home)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[derive(Clone, Debug)]
struct LegacyLayout {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
    documents_dir: PathBuf,
    pictures_dirs: Vec<PathBuf>,
}

impl LegacyLayout {
    fn exists(&self) -> Result<bool> {
        for path in [
            &self.config_dir,
            &self.data_dir,
            &self.cache_dir,
            &self.state_dir,
            &self.documents_dir,
        ]
        .into_iter()
        .chain(self.pictures_dirs.iter())
        {
            if entry_exists(path)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[derive(Clone, Debug)]
struct Layout {
    root_dir: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
}

impl Layout {
    fn marker(&self) -> PathBuf {
        self.root_dir.join(LAYOUT_MARKER)
    }

    fn resource_marker(&self) -> PathBuf {
        self.root_dir.join(RESOURCE_LAYOUT_MARKER)
    }

    fn resource_journal(&self) -> PathBuf {
        self.root_dir.join(RESOURCE_MIGRATION_JOURNAL)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResourceMigrationEntry {
    source: PathBuf,
    destination: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResourceMigrationJournal {
    entries: Vec<ResourceMigrationEntry>,
    moved: usize,
    #[serde(default)]
    pending: Option<usize>,
}

fn resource_layout_entries(layout: &Layout) -> Vec<ResourceMigrationEntry> {
    vec![
        ResourceMigrationEntry {
            source: layout.config_dir.join("skills"),
            destination: layout.data_dir.join("skills"),
        },
        ResourceMigrationEntry {
            source: layout.config_dir.join("scripts"),
            destination: layout.data_dir.join("scripts"),
        },
        ResourceMigrationEntry {
            source: layout.config_dir.join("prompts"),
            destination: layout.data_dir.join("prompts"),
        },
        ResourceMigrationEntry {
            source: layout.config_dir.join("identities"),
            destination: layout.data_dir.join("identities"),
        },
        ResourceMigrationEntry {
            source: layout.config_dir.join("persona-avatars"),
            destination: layout.data_dir.join("persona-avatars"),
        },
        ResourceMigrationEntry {
            source: layout.config_dir.join("system-prompt.md"),
            destination: layout.data_dir.join("prompts/system-prompt.md"),
        },
        ResourceMigrationEntry {
            source: layout.config_dir.join("user-identity.md"),
            destination: layout.data_dir.join("identities/user-identity.md"),
        },
    ]
}

fn current_process_is_daemon() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "__daemon")
}

fn resource_runtime_dir(layout: &Layout) -> PathBuf {
    if cfg!(test) {
        return layout.state_dir.join("miyu");
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime_root) => runtime_dir_for(
            Path::new(&runtime_root),
            std::env::var_os("MIYU_HOME").as_deref().map(Path::new),
        ),
        None => layout.state_dir.join("miyu"),
    }
}

fn daemon_is_running_at(runtime_dir: &Path, current_process_is_daemon: bool) -> bool {
    let sock_connected = {
        #[cfg(unix)]
        {
            std::os::unix::net::UnixStream::connect(runtime_dir.join("core.sock")).is_ok()
        }
        #[cfg(not(unix))]
        {
            false
        }
    };
    sock_connected
        || runtime_lock_is_held(&runtime_dir.join("core.lock")).unwrap_or(false)
        || (!current_process_is_daemon && runtime_lock_is_held(&runtime_dir.join("starter.lock")).unwrap_or(false))
}

fn normalize_resource_relative_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn resource_layout_marker_exists(layout: &Layout) -> Result<bool> {
    marker_exists_at(&layout.resource_marker(), "resource layout marker")
}

fn marker_exists_at(path: &Path, label: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "Miyu {label} must not be a symbolic link: {}",
                path.display()
            )
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("Miyu {label} is not a regular file: {}", path.display())
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn migrate_resource_layout(layout: &Layout) -> Result<()> {
    if !try_migrate_resource_layout(layout, false)? {
        bail!("Miyu resource migration is deferred while another daemon or starter is active");
    }
    Ok(())
}

fn try_migrate_resource_layout(layout: &Layout, current_process_is_daemon: bool) -> Result<bool> {
    if resource_layout_marker_exists(layout)? {
        remove_resource_journal_if_present(layout)?;
        return Ok(true);
    }

    ensure_private_dir(&layout.root_dir)?;
    ensure_private_dir(&layout.config_dir)?;
    ensure_private_dir(&layout.data_dir)?;
    let _lease = acquire_migration_lock(&layout.root_dir)?;
    if resource_layout_marker_exists(layout)? {
        remove_resource_journal_if_present(layout)?;
        return Ok(true);
    }
    let Some(_daemon_guard) = try_acquire_resource_daemon_guard(layout, current_process_is_daemon)?
    else {
        return Ok(false);
    };

    recover_resource_migration(layout)?;
    let mut entries = Vec::new();
    for entry in resource_layout_entries(layout) {
        if entry_exists(&entry.source)? {
            entries.push(entry);
        }
    }
    preflight_resource_entries(layout, &entries)?;

    let mut journal = ResourceMigrationJournal {
        entries,
        moved: 0,
        pending: None,
    };
    write_resource_journal(layout, &journal)?;
    while journal.moved < journal.entries.len() {
        let index = journal.moved;
        let entry = journal.entries[index].clone();
        journal.pending = Some(index);
        write_resource_journal(layout, &journal)?;
        if let Err(error) = atomic_resource_move(&entry.source, &entry.destination) {
            let recovery = recover_resource_migration(layout);
            return match recovery {
                Ok(()) => Err(error).with_context(|| {
                    format!(
                        "migrating Miyu resource from {} to {}",
                        entry.source.display(),
                        entry.destination.display()
                    )
                }),
                Err(recovery_error) => Err(anyhow::anyhow!(
                    "resource migration failed: {error:#}; rollback also failed: {recovery_error:#}"
                )),
            };
        }
        journal.moved = index + 1;
        journal.pending = None;
        write_resource_journal(layout, &journal)?;
    }

    write_marker(&layout.resource_marker())?;
    remove_resource_journal_if_present(layout)?;
    Ok(true)
}

struct ResourceDaemonGuard {
    _starter: Option<File>,
    _core: File,
}

fn try_acquire_resource_daemon_guard(
    layout: &Layout,
    current_process_is_daemon: bool,
) -> Result<Option<ResourceDaemonGuard>> {
    let runtime_dir = resource_runtime_dir(layout);
    ensure_private_dir(&runtime_dir)?;
    let starter_path = runtime_dir.join("starter.lock");
    let starter_already_held = current_process_is_daemon && runtime_lock_is_held(&starter_path).unwrap_or(false);
    let starter = if starter_already_held {
        None
    } else {
        let Some(lock) = try_acquire_runtime_lock(&starter_path)? else {
            return Ok(None);
        };
        Some(lock)
    };
    let Some(core) = try_acquire_runtime_lock(&runtime_dir.join("core.lock"))? else {
        return Ok(None);
    };
    let unix_sock_ok = {
        #[cfg(unix)]
        {
            std::os::unix::net::UnixStream::connect(runtime_dir.join("core.sock")).is_ok()
        }
        #[cfg(not(unix))]
        {
            false
        }
    };
    if unix_sock_ok {
        return Ok(None);
    }
    Ok(Some(ResourceDaemonGuard {
        _starter: starter,
        _core: core,
    }))
}

fn try_acquire_runtime_lock(path: &Path) -> Result<Option<File>> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    sys::set_open_options_mode(&mut options, 0o600);
    let file = options.open(path)?;
    if sys::flock_lock_ex(&file, true)? {
        return Ok(Some(file));
    }
    Ok(None)
}

fn preflight_resource_entries(layout: &Layout, entries: &[ResourceMigrationEntry]) -> Result<()> {
    let projections = entries
        .iter()
        .map(|entry| (entry.source.clone(), entry.destination.clone()))
        .collect::<Vec<_>>();
    for entry in entries {
        let source_metadata = fs::symlink_metadata(&entry.source)?;
        if source_metadata.file_type().is_symlink() {
            bail!(
                "Miyu resource migration refuses symbolic-link source {}",
                entry.source.display()
            );
        }
        ensure_supported_entry_tree(&entry.source)?;
        ensure_absolute_symlink_targets_stable(&entry.source, &projections)?;
        ensure_destination_ancestors(layout, &entry.destination)?;
        ensure_resource_same_filesystem(&entry.source, &entry.destination)?;
        match fs::symlink_metadata(&entry.destination) {
            Ok(_) => bail!(
                "Miyu resource migration destination already exists: {}; move or remove it and retry",
                entry.destination.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    for (index, parent) in entries.iter().enumerate() {
        for child in entries.iter().skip(index + 1) {
            ensure_resource_targets_do_not_overlap(parent, child)?;
            ensure_resource_targets_do_not_overlap(child, parent)?;
        }
    }
    Ok(())
}

fn ensure_destination_ancestors(layout: &Layout, destination: &Path) -> Result<()> {
    let relative = destination
        .strip_prefix(&layout.data_dir)
        .with_context(|| {
            format!(
                "resource destination escapes Miyu data directory: {}",
                destination.display()
            )
        })?;
    let mut current = layout.data_dir.clone();
    for component in relative.components() {
        current.push(component);
        if current == destination {
            break;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "Miyu resource migration refuses symbolic-link destination ancestor {}",
                current.display()
            ),
            Ok(metadata) if !metadata.is_dir() => bail!(
                "Miyu resource migration destination ancestor is not a directory: {}",
                current.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_resource_same_filesystem(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let source_device = fs::symlink_metadata(source)?.dev();
    let mut ancestor = destination
        .parent()
        .context("resource destination has no parent")?;
    let destination_device = loop {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => break metadata.dev(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .context("resource destination has no existing ancestor")?;
            }
            Err(error) => return Err(error.into()),
        }
    };
    if source_device != destination_device {
        bail!(
            "Miyu resource migration requires source and destination on the same filesystem: {} -> {}",
            source.display(),
            destination.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_resource_same_filesystem(_source: &Path, _destination: &Path) -> Result<()> {
    Ok(())
}

fn ensure_resource_targets_do_not_overlap(
    parent: &ResourceMigrationEntry,
    child: &ResourceMigrationEntry,
) -> Result<()> {
    let Ok(relative) = child.destination.strip_prefix(&parent.destination) else {
        return Ok(());
    };
    if relative.as_os_str().is_empty() {
        return Ok(());
    }
    let nested_source = parent.source.join(relative);
    if entry_exists(&nested_source)? && entry_exists(&child.source)? {
        bail!(
            "Miyu resource migration found overlapping sources for {} and {}; remove one duplicate and retry",
            nested_source.display(),
            child.source.display()
        );
    }
    Ok(())
}

fn atomic_resource_move(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(source, destination)?;
    sync_parent(destination)?;
    if source.parent() != destination.parent() {
        sync_parent(source)?;
    }
    Ok(())
}

fn write_resource_journal(layout: &Layout, journal: &ResourceMigrationJournal) -> Result<()> {
    let path = layout.resource_journal();
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    sys::set_open_options_mode(&mut options, 0o600);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    serde_json::to_writer_pretty(&mut file, journal)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("installing resource migration journal {}", path.display()))?;
    sync_parent(&path)
}

fn recover_resource_migration(layout: &Layout) -> Result<()> {
    let path = layout.resource_journal();
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut journal: ResourceMigrationJournal = serde_json::from_str(&raw)
        .with_context(|| format!("parsing resource migration journal {}", path.display()))?;
    if journal.moved > journal.entries.len() {
        bail!("invalid Miyu resource migration journal: moved count is out of range");
    }
    if let Some(index) = journal.pending {
        if index != journal.moved || index >= journal.entries.len() {
            bail!("invalid Miyu resource migration journal: pending index is out of range");
        }
        let entry = &journal.entries[index];
        match (
            entry_exists(&entry.source)?,
            entry_exists(&entry.destination)?,
        ) {
            (true, false) => journal.pending = None,
            (false, true) => {
                journal.moved = index + 1;
                journal.pending = None;
            }
            (true, true) => bail!(
                "cannot recover Miyu resource migration because both paths exist: {} and {}",
                entry.source.display(),
                entry.destination.display()
            ),
            (false, false) => bail!(
                "cannot recover Miyu resource migration because both paths are missing: {} and {}",
                entry.source.display(),
                entry.destination.display()
            ),
        }
        write_resource_journal(layout, &journal)?;
    }
    while journal.moved > 0 {
        let index = journal.moved - 1;
        let entry = journal.entries[index].clone();
        match (
            entry_exists(&entry.source)?,
            entry_exists(&entry.destination)?,
        ) {
            (false, true) => {
                atomic_resource_move(&entry.destination, &entry.source).with_context(|| {
                    format!(
                        "rolling back Miyu resource migration from {} to {}",
                        entry.destination.display(),
                        entry.source.display()
                    )
                })?
            }
            (true, false) => {}
            (true, true) => bail!(
                "cannot recover Miyu resource migration because both paths exist: {} and {}",
                entry.source.display(),
                entry.destination.display()
            ),
            (false, false) => bail!(
                "cannot recover Miyu resource migration because both paths are missing: {} and {}",
                entry.source.display(),
                entry.destination.display()
            ),
        }
        journal.moved = index;
        write_resource_journal(layout, &journal)?;
    }
    remove_resource_journal_if_present(layout)
}

fn remove_resource_journal_if_present(layout: &Layout) -> Result<()> {
    let path = layout.resource_journal();
    match fs::remove_file(&path) {
        Ok(()) => sync_parent(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Debug)]
struct MigrationMapping {
    source: PathBuf,
    destination: PathBuf,
}

impl MigrationMapping {
    fn new(source: impl Into<PathBuf>, destination: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            destination: destination.into(),
        }
    }
}

struct MigrationLease(File);

impl Drop for MigrationLease {
    fn drop(&mut self) {
        sys::flock_unlock(&self.0);
    }
}

fn legacy_daemon_is_running(legacy: &LegacyLayout) -> bool {
    legacy_daemon_is_running_at(
        legacy,
        std::env::var_os("XDG_RUNTIME_DIR")
            .as_deref()
            .map(Path::new),
    )
}

fn legacy_daemon_is_running_at(legacy: &LegacyLayout, xdg_runtime_dir: Option<&Path>) -> bool {
    let mut runtime_dirs = vec![legacy.state_dir.clone()];
    if let Some(runtime_dir) = xdg_runtime_dir {
        runtime_dirs.push(runtime_dir.to_path_buf());
    }
    runtime_dirs
        .into_iter()
        .map(|runtime_dir| runtime_dir.join("miyu"))
        .any(|runtime_dir| {
            let unix_sock = {
                #[cfg(unix)]
                {
                    std::os::unix::net::UnixStream::connect(runtime_dir.join("core.sock")).is_ok()
                }
                #[cfg(not(unix))]
                {
                    false
                }
            };
            unix_sock
                || runtime_lock_is_held(&runtime_dir.join("core.lock")).unwrap_or(false)
                || runtime_lock_is_held(&runtime_dir.join("starter.lock")).unwrap_or(false)
        })
}

fn runtime_lock_is_held(path: &Path) -> Result<bool> {
    let file = match OpenOptions::new().read(true).write(true).create(false).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
            || error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    match sys::flock_lock_ex(&file, true) {
        Ok(true) => {
            sys::flock_unlock(&file);
            Ok(false)
        }
        _ => Ok(true),
    }
}

fn migrate_legacy_layout(legacy: &LegacyLayout, next: &Layout) -> Result<()> {
    if layout_marker_exists(next)? {
        return Ok(());
    }
    let mappings = legacy_migration_mappings(legacy, next);
    let initial = existing_mappings(&mappings)?;
    preflight_mappings(&initial)?;

    ensure_private_dir(&next.root_dir)?;
    for directory in [
        &next.config_dir,
        &next.data_dir,
        &next.cache_dir,
        &next.state_dir,
    ] {
        ensure_existing_directory(directory)?;
    }
    let _lease = acquire_migration_lock(&next.root_dir)?;
    if layout_marker_exists(next)? {
        return Ok(());
    }
    // Repeat the full preflight while holding the layout lock. The first pass
    // guarantees a known conflict does not even create ~/.miyu; this pass
    // closes the race with another new Miyu process before any data moves.
    let active = existing_mappings(&mappings)?;
    preflight_mappings(&active)?;
    let next_bash_hook = next.config_dir.join("shell/bash-hook.sh");
    let next_zsh_hook = next.config_dir.join("shell/zsh-hook.zsh");
    let had_bash_hook = entry_exists(&legacy.config_dir.join("shell/bash-hook.sh"))?
        || entry_exists(&next_bash_hook)?;
    let had_zsh_hook = entry_exists(&legacy.config_dir.join("shell/zsh-hook.zsh"))?
        || entry_exists(&next_zsh_hook)?;
    for mapping in &active {
        migrate_entry_unchecked(&mapping.source, &mapping.destination).with_context(|| {
            format!(
                "migrating Miyu user files from {} to {}",
                mapping.source.display(),
                mapping.destination.display()
            )
        })?;
    }

    if had_bash_hook || had_zsh_hook {
        let home = next
            .root_dir
            .parent()
            .context("the Miyu home directory has no parent")?;
        let bash_hook = had_bash_hook.then_some(next_bash_hook);
        let zsh_hook = had_zsh_hook.then_some(next_zsh_hook);
        crate::shell::refresh_migrated_hook_sources(
            home,
            bash_hook.as_deref(),
            zsh_hook.as_deref(),
        )
        .context("refreshing shell hook paths after Miyu directory migration")?;
    }

    write_marker(&next.marker())?;
    Ok(())
}

fn legacy_migration_mappings(legacy: &LegacyLayout, next: &Layout) -> Vec<MigrationMapping> {
    let mut mappings = vec![
        MigrationMapping::new(&legacy.config_dir, &next.config_dir),
        MigrationMapping::new(&legacy.data_dir, &next.data_dir),
        MigrationMapping::new(&legacy.cache_dir, &next.cache_dir),
        MigrationMapping::new(&legacy.documents_dir, next.data_dir.join("documents")),
    ];
    // Older installations may have no XDG state directory and therefore use
    // the data directory for both values. Mapping the same source twice would
    // make the migration appear self-conflicting; keep its contents under the
    // unified data directory and leave the new state directory empty.
    if legacy.state_dir != legacy.data_dir {
        mappings.push(MigrationMapping::new(&legacy.state_dir, &next.state_dir));
    }
    mappings.extend(
        legacy
            .pictures_dirs
            .iter()
            .map(|source| MigrationMapping::new(source, next.data_dir.join("pictures"))),
    );
    mappings
}

fn existing_mappings(mappings: &[MigrationMapping]) -> Result<Vec<MigrationMapping>> {
    let mut active = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        if mapping.source == mapping.destination || !entry_exists(&mapping.source)? {
            continue;
        }
        if mapping.destination.starts_with(&mapping.source)
            || mapping.source.starts_with(&mapping.destination)
        {
            bail!(
                "Miyu directory migration cannot move overlapping paths: {} and {}",
                mapping.source.display(),
                mapping.destination.display()
            );
        }
        active.push(mapping.clone());
    }
    for (index, left) in active.iter().enumerate() {
        for right in active.iter().skip(index + 1) {
            if left.source == right.source
                || left.source.starts_with(&right.source)
                || right.source.starts_with(&left.source)
            {
                bail!(
                    "Miyu directory migration has overlapping legacy sources: {} and {}",
                    left.source.display(),
                    right.source.display()
                );
            }
        }
    }
    Ok(active)
}

fn entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
            || error.kind() == std::io::ErrorKind::PermissionDenied => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn acquire_migration_lock(root: &Path) -> Result<MigrationLease> {
    // Lock the directory itself on Unix, or a lockfile on Windows so opening directory doesn't fail with OS Error 5.
    #[cfg(unix)]
    let file = File::open(root)
        .with_context(|| format!("opening migration lock directory {}", root.display()))?;
    #[cfg(not(unix))]
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(".migration.lock"))
        .with_context(|| format!("opening migration lock file in {}", root.display()))?;

    if sys::flock_lock_ex(&file, false)? {
        Ok(MigrationLease(file))
    } else {
        Err(std::io::Error::last_os_error()).context("locking Miyu directory migration")
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    ensure_existing_directory(path)?;
    if !entry_exists(path)? {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        sys::set_dir_builder_mode(&mut builder, 0o700);
        builder
            .create(path)
            .with_context(|| format!("creating {}", path.display()))?;
    }
    ensure_existing_directory(path)?;
    sys::set_secure_dir_permissions(path)
        .with_context(|| format!("securing {}", path.display()))?;
    Ok(())
}

fn ensure_existing_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "Miyu refuses to use a symbolic-link directory: {}",
            path.display()
        ),
        Ok(metadata) if !metadata.is_dir() => bail!(
            "Miyu expected a directory but found another file: {}",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn layout_marker_exists(layout: &Layout) -> Result<bool> {
    let marker = layout.marker();
    match fs::symlink_metadata(&marker) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "Miyu layout marker must not be a symbolic link: {}",
            marker.display()
        ),
        Ok(metadata) if !metadata.is_file() => bail!(
            "Miyu layout marker is not a regular file: {}",
            marker.display()
        ),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
            || error.kind() == std::io::ErrorKind::PermissionDenied => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn write_marker(path: &Path) -> Result<()> {
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    sys::set_open_options_mode(&mut options, 0o600);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    file.write_all(b"1\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)
        .with_context(|| format!("installing migration marker {}", path.display()))?;
    sync_parent(path)?;
    Ok(())
}

fn migrate_entry(source: &Path, destination: &Path) -> Result<()> {
    let mappings = existing_mappings(&[MigrationMapping::new(source, destination)])?;
    preflight_mappings(&mappings)?;
    let Some(mapping) = mappings.first() else {
        return Ok(());
    };
    migrate_entry_unchecked(&mapping.source, &mapping.destination)
}

fn preflight_mappings(mappings: &[MigrationMapping]) -> Result<()> {
    let projections = mappings
        .iter()
        .map(|mapping| (mapping.source.clone(), mapping.destination.clone()))
        .collect::<Vec<_>>();
    for mapping in mappings {
        ensure_supported_entry_tree(&mapping.source)?;
        ensure_absolute_symlink_targets_stable(&mapping.source, &projections)?;
        ensure_no_conflicts(&mapping.source, &mapping.destination)?;
    }
    for (index, left) in mappings.iter().enumerate() {
        for right in mappings.iter().skip(index + 1) {
            ensure_mapping_pair_compatible(left, right)?;
        }
    }
    Ok(())
}

fn ensure_supported_entry_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        if target.is_relative() {
            bail!(
                "Miyu directory migration refuses relative symbolic link {}; its target would change after moving",
                path.display()
            );
        }
        return Ok(());
    }
    if metadata.is_file() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            ensure_supported_entry_tree(&entry?.path())?;
        }
        return Ok(());
    }
    bail!("unsupported file type while migrating {}", path.display())
}

fn ensure_absolute_symlink_targets_stable(
    path: &Path,
    projections: &[(PathBuf, PathBuf)],
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        if target.is_absolute() {
            for (source, destination) in projections {
                if let Ok(relative) = target.strip_prefix(source) {
                    bail!(
                        "Miyu directory migration refuses symbolic link {} because its absolute target moves from {} to {}",
                        path.display(),
                        target.display(),
                        destination.join(relative).display()
                    );
                }
            }
        }
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            ensure_absolute_symlink_targets_stable(&entry?.path(), projections)?;
        }
    }
    Ok(())
}

fn ensure_mapping_pair_compatible(left: &MigrationMapping, right: &MigrationMapping) -> Result<()> {
    if left.destination == right.destination {
        return ensure_projected_entries_compatible(&left.source, &right.source, &left.destination);
    }
    if let Ok(relative) = right.destination.strip_prefix(&left.destination) {
        return ensure_nested_mapping_compatible(left, relative, right);
    }
    if let Ok(relative) = left.destination.strip_prefix(&right.destination) {
        return ensure_nested_mapping_compatible(right, relative, left);
    }
    Ok(())
}

fn ensure_nested_mapping_compatible(
    outer: &MigrationMapping,
    relative_destination: &Path,
    inner: &MigrationMapping,
) -> Result<()> {
    let Some(projected_source) = projected_source_entry(&outer.source, relative_destination)?
    else {
        return Ok(());
    };
    ensure_projected_entries_compatible(&projected_source, &inner.source, &inner.destination)
}

/// Locates the source entry which an outer mapping would project onto a nested
/// destination. A non-directory ancestor is already a conflict because the
/// inner mapping needs that destination path to remain traversable.
fn projected_source_entry(source_root: &Path, relative: &Path) -> Result<Option<PathBuf>> {
    let mut current = source_root.to_path_buf();
    for component in relative.components() {
        let metadata = fs::symlink_metadata(&current)?;
        if !metadata.is_dir() {
            bail!(
                "Miyu directory migration found a projected path conflict at {}",
                current.display()
            );
        }
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Some(current))
}

fn ensure_projected_entries_compatible(
    left: &Path,
    right: &Path,
    projected_destination: &Path,
) -> Result<()> {
    let left_meta = fs::symlink_metadata(left)?;
    let right_meta = fs::symlink_metadata(right)?;
    if left_meta.is_dir() && right_meta.is_dir() {
        for entry in fs::read_dir(left)? {
            let entry = entry?;
            let right_child = right.join(entry.file_name());
            match fs::symlink_metadata(&right_child) {
                Ok(_) => ensure_projected_entries_compatible(
                    &entry.path(),
                    &right_child,
                    &projected_destination.join(entry.file_name()),
                )?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        return Ok(());
    }
    if entries_identical(left, &left_meta, right, &right_meta)? {
        return Ok(());
    }
    bail!(
        "Miyu directory migration found conflicting legacy entries {} and {} projected to {}",
        left.display(),
        right.display(),
        projected_destination.display()
    )
}

fn ensure_no_conflicts(source: &Path, destination: &Path) -> Result<()> {
    let source_meta = fs::symlink_metadata(source)?;
    match fs::symlink_metadata(destination) {
        Ok(destination_meta) => {
            if source_meta.is_dir() && destination_meta.is_dir() {
                for entry in fs::read_dir(source)? {
                    let entry = entry?;
                    ensure_no_conflicts(&entry.path(), &destination.join(entry.file_name()))?;
                }
                return Ok(());
            }
            if entries_identical(source, &source_meta, destination, &destination_meta)? {
                return Ok(());
            }
            bail!(
                "Miyu directory migration found conflicting entries: {} and {}; move or rename one of them and retry",
                source.display(),
                destination.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn migrate_entry_unchecked(source: &Path, destination: &Path) -> Result<()> {
    let source_meta = fs::symlink_metadata(source)?;
    match fs::symlink_metadata(destination) {
        Ok(destination_meta) => {
            if source_meta.is_dir() && destination_meta.is_dir() {
                for entry in fs::read_dir(source)? {
                    let entry = entry?;
                    migrate_entry_unchecked(&entry.path(), &destination.join(entry.file_name()))?;
                }
                remove_empty_dir(source)?;
                return Ok(());
            }
            if entries_identical(source, &source_meta, destination, &destination_meta)? {
                remove_entry(source, &source_meta)?;
                return Ok(());
            }
            bail!(
                "Miyu directory migration found a conflict that appeared after preflight: {} and {}",
                source.display(),
                destination.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(source, destination) {
        Ok(()) => {
            sync_parent(destination)?;
            if source.parent() != destination.parent() {
                sync_parent(source)?;
            }
            Ok(())
        }
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            copy_entry(source, &source_meta, destination)?;
            remove_entry(source, &source_meta)
        }
        Err(error) => Err(error.into()),
    }
}

fn copy_entry(source: &Path, metadata: &fs::Metadata, destination: &Path) -> Result<()> {
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source)?;
        symlink(target, destination)?;
        sync_parent(destination)?;
        return Ok(());
    }
    if metadata.is_dir() {
        fs::create_dir(destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let child_meta = fs::symlink_metadata(entry.path())?;
            copy_entry(
                &entry.path(),
                &child_meta,
                &destination.join(entry.file_name()),
            )?;
        }
        #[cfg(unix)]
        let _ = File::open(destination).map(|f| f.sync_all());
        sync_parent(destination)?;
        return Ok(());
    }
    if !metadata.is_file() {
        bail!("unsupported file type while migrating {}", source.display());
    }
    let temporary = destination.with_extension(format!(
        "miyu-migrate-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut input = File::open(source)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    sys::set_open_options_mode(&mut options, sys::get_permissions_mode(&metadata.permissions()));
    let mut output = options.open(&temporary)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    fs::rename(&temporary, destination)?;
    sync_parent(destination)?;
    Ok(())
}

fn sync_parent(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(parent) = _path.parent() {
        if let Ok(file) = File::open(parent) {
            let _ = file.sync_all();
        }
    }
    Ok(())
}

fn entries_identical(
    left: &Path,
    left_meta: &fs::Metadata,
    right: &Path,
    right_meta: &fs::Metadata,
) -> Result<bool> {
    if left_meta.file_type().is_symlink() || right_meta.file_type().is_symlink() {
        return Ok(left_meta.file_type().is_symlink()
            && right_meta.file_type().is_symlink()
            && fs::read_link(left)? == fs::read_link(right)?);
    }
    if !left_meta.is_file()
        || !right_meta.is_file()
        || left_meta.len() != right_meta.len()
        || sys::get_permissions_mode(&left_meta.permissions()) != sys::get_permissions_mode(&right_meta.permissions())
    {
        return Ok(false);
    }
    let mut left_file = File::open(left)?;
    let mut right_file = File::open(right)?;
    let mut left_buffer = [0u8; 64 * 1024];
    let mut right_buffer = [0u8; 64 * 1024];
    loop {
        let left_count = left_file.read(&mut left_buffer)?;
        let right_count = right_file.read(&mut right_buffer)?;
        if left_count != right_count || left_buffer[..left_count] != right_buffer[..right_count] {
            return Ok(false);
        }
        if left_count == 0 {
            return Ok(true);
        }
    }
}

fn remove_entry(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    sync_parent(path)
}

fn remove_empty_dir(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_layouts(root: &Path) -> (LegacyLayout, Layout) {
        (
            LegacyLayout {
                config_dir: root.join("legacy/config"),
                data_dir: root.join("legacy/data"),
                cache_dir: root.join("legacy/cache"),
                state_dir: root.join("legacy/state"),
                documents_dir: root.join("Documents/Miyu"),
                pictures_dirs: vec![root.join("Pictures/miyu"), root.join("Pictures/Miyu")],
            },
            Layout {
                root_dir: root.join(".miyu"),
                config_dir: root.join(".miyu/config"),
                data_dir: root.join(".miyu/data"),
                cache_dir: root.join(".miyu/cache"),
                state_dir: root.join(".miyu/state"),
            },
        )
    }

    #[test]
    fn resource_layout_migration_moves_owned_content_and_commits_marker() {
        let temp = tempfile::tempdir().unwrap();
        let (_, layout) = test_layouts(temp.path());
        fs::create_dir_all(layout.config_dir.join("skills/demo")).unwrap();
        fs::create_dir_all(layout.config_dir.join("scripts")).unwrap();
        fs::create_dir_all(layout.config_dir.join("prompts")).unwrap();
        fs::create_dir_all(layout.config_dir.join("identities")).unwrap();
        fs::create_dir_all(layout.config_dir.join("persona-avatars")).unwrap();
        fs::write(
            layout.config_dir.join("skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo\n---\n",
        )
        .unwrap();
        fs::write(layout.config_dir.join("scripts/tool.sh"), "#!/bin/sh\n").unwrap();
        fs::write(layout.config_dir.join("prompts/persona.md"), "persona").unwrap();
        fs::write(layout.config_dir.join("identities/user.md"), "user").unwrap();
        fs::write(
            layout.config_dir.join("persona-avatars/avatar.png"),
            "image",
        )
        .unwrap();
        fs::write(layout.config_dir.join("system-prompt.md"), "system").unwrap();
        fs::write(layout.config_dir.join("user-identity.md"), "legacy user").unwrap();

        migrate_resource_layout(&layout).unwrap();

        assert!(layout.data_dir.join("skills/demo/SKILL.md").is_file());
        assert!(layout.data_dir.join("scripts/tool.sh").is_file());
        assert!(layout.data_dir.join("prompts/persona.md").is_file());
        assert!(layout.data_dir.join("prompts/system-prompt.md").is_file());
        assert!(layout.data_dir.join("identities/user.md").is_file());
        assert!(layout
            .data_dir
            .join("identities/user-identity.md")
            .is_file());
        assert!(layout.data_dir.join("persona-avatars/avatar.png").is_file());
        assert!(layout.resource_marker().is_file());
        assert!(!layout.resource_journal().exists());
    }

    #[test]
    fn resource_layout_conflict_has_no_migration_writes() {
        let temp = tempfile::tempdir().unwrap();
        let (_, layout) = test_layouts(temp.path());
        fs::create_dir_all(layout.config_dir.join("skills/demo")).unwrap();
        fs::create_dir_all(layout.data_dir.join("skills/existing")).unwrap();
        fs::write(layout.config_dir.join("skills/demo/SKILL.md"), "source").unwrap();
        fs::write(
            layout.data_dir.join("skills/existing/SKILL.md"),
            "destination",
        )
        .unwrap();

        let error = migrate_resource_layout(&layout).unwrap_err();

        assert!(error.to_string().contains("destination already exists"));
        assert!(layout.config_dir.join("skills/demo/SKILL.md").is_file());
        assert!(layout.data_dir.join("skills/existing/SKILL.md").is_file());
        assert!(!layout.resource_marker().exists());
        assert!(!layout.resource_journal().exists());
    }

    #[test]
    fn resource_layout_journal_rolls_back_interrupted_moves() {
        let temp = tempfile::tempdir().unwrap();
        let (_, layout) = test_layouts(temp.path());
        let source = layout.config_dir.join("skills");
        let destination = layout.data_dir.join("skills");
        fs::create_dir_all(source.join("demo")).unwrap();
        fs::create_dir_all(&layout.data_dir).unwrap();
        fs::write(source.join("demo/SKILL.md"), "skill").unwrap();
        fs::rename(&source, &destination).unwrap();
        let journal = ResourceMigrationJournal {
            entries: vec![ResourceMigrationEntry {
                source: source.clone(),
                destination: destination.clone(),
            }],
            moved: 1,
            pending: None,
        };
        fs::create_dir_all(&layout.root_dir).unwrap();
        write_resource_journal(&layout, &journal).unwrap();

        recover_resource_migration(&layout).unwrap();

        assert!(source.join("demo/SKILL.md").is_file());
        assert!(!destination.exists());
        assert!(!layout.resource_journal().exists());
    }

    #[test]
    fn resource_layout_journal_recovers_pending_and_already_rolled_back_entries() {
        let temp = tempfile::tempdir().unwrap();
        let (_, layout) = test_layouts(temp.path());
        let source = layout.config_dir.join("skills");
        let destination = layout.data_dir.join("skills");
        fs::create_dir_all(source.join("demo")).unwrap();
        fs::create_dir_all(&layout.data_dir).unwrap();
        fs::write(source.join("demo/SKILL.md"), "skill").unwrap();
        fs::rename(&source, &destination).unwrap();
        let mut journal = ResourceMigrationJournal {
            entries: vec![ResourceMigrationEntry {
                source: source.clone(),
                destination: destination.clone(),
            }],
            moved: 0,
            pending: Some(0),
        };
        fs::create_dir_all(&layout.root_dir).unwrap();
        write_resource_journal(&layout, &journal).unwrap();
        recover_resource_migration(&layout).unwrap();
        assert!(source.join("demo/SKILL.md").is_file());

        journal.moved = 1;
        journal.pending = None;
        write_resource_journal(&layout, &journal).unwrap();
        recover_resource_migration(&layout).unwrap();
        assert!(source.join("demo/SKILL.md").is_file());
        assert!(!destination.exists());
        assert!(!layout.resource_journal().exists());
    }

    #[test]
    fn resource_layout_preflights_overlapping_legacy_files() {
        let temp = tempfile::tempdir().unwrap();
        let (_, layout) = test_layouts(temp.path());
        fs::create_dir_all(layout.config_dir.join("prompts")).unwrap();
        fs::write(layout.config_dir.join("prompts/system-prompt.md"), "nested").unwrap();
        fs::write(layout.config_dir.join("system-prompt.md"), "top-level").unwrap();

        let error = migrate_resource_layout(&layout).unwrap_err();

        assert!(error.to_string().contains("overlapping sources"));
        assert!(layout.config_dir.join("prompts/system-prompt.md").is_file());
        assert!(layout.config_dir.join("system-prompt.md").is_file());
        assert!(!layout.resource_journal().exists());
    }

    #[cfg(unix)]
    #[test]
    fn resource_layout_rejects_symbolic_link_destination_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let (_, layout) = test_layouts(temp.path());
        let outside = temp.path().join("outside");
        fs::create_dir_all(&layout.config_dir).unwrap();
        fs::create_dir_all(&layout.data_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(layout.config_dir.join("system-prompt.md"), "system").unwrap();
        symlink(&outside, layout.data_dir.join("prompts")).unwrap();

        let error = migrate_resource_layout(&layout).unwrap_err();

        assert!(error
            .to_string()
            .contains("symbolic-link destination ancestor"));
        assert!(layout.config_dir.join("system-prompt.md").is_file());
        assert!(!outside.join("system-prompt.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn resource_layout_rejects_absolute_symlinks_into_moved_trees() {
        let temp = tempfile::tempdir().unwrap();
        let (_, layout) = test_layouts(temp.path());
        let skill_file = layout.config_dir.join("skills/demo/SKILL.md");
        fs::create_dir_all(skill_file.parent().unwrap()).unwrap();
        fs::create_dir_all(layout.config_dir.join("prompts")).unwrap();
        fs::write(&skill_file, "skill").unwrap();
        symlink(
            &skill_file,
            layout.config_dir.join("prompts/linked-skill.md"),
        )
        .unwrap();

        let error = migrate_resource_layout(&layout).unwrap_err();

        assert!(error.to_string().contains("absolute target moves"));
        assert!(skill_file.is_file());
        assert!(!layout.resource_marker().exists());
    }

    #[test]
    fn migration_moves_and_merges_without_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("legacy");
        let destination = temp.path().join("next");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("nested/value.txt"), "old").unwrap();
        fs::write(destination.join("kept.txt"), "new").unwrap();

        migrate_entry(&source, &destination).unwrap();

        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(destination.join("nested/value.txt")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(destination.join("kept.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn migration_preflights_conflicts_then_removes_identical_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("legacy");
        let destination = temp.path().join("next");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("same.txt"), "same").unwrap();
        fs::write(destination.join("same.txt"), "same").unwrap();
        fs::write(source.join("conflict.txt"), "legacy").unwrap();
        fs::write(destination.join("conflict.txt"), "next").unwrap();

        let error = migrate_entry(&source, &destination).unwrap_err();

        assert!(source.join("same.txt").exists());
        assert!(source.join("conflict.txt").exists());
        assert_eq!(
            fs::read_to_string(destination.join("conflict.txt")).unwrap(),
            "next"
        );
        assert!(error.to_string().contains("conflicting entries"));

        fs::remove_file(source.join("conflict.txt")).unwrap();
        migrate_entry(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(destination.join("same.txt")).unwrap(),
            "same"
        );
    }

    #[test]
    fn unified_layout_merges_xdg_documents_and_both_picture_directories() {
        let temp = tempfile::tempdir().unwrap();
        let (legacy, next) = test_layouts(temp.path());
        for directory in [
            &legacy.config_dir,
            &legacy.data_dir,
            &legacy.cache_dir,
            &legacy.state_dir,
            &legacy.documents_dir,
            &legacy.pictures_dirs[0],
            &legacy.pictures_dirs[1],
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::create_dir_all(legacy.data_dir.join("documents")).unwrap();
        fs::create_dir_all(legacy.data_dir.join("pictures")).unwrap();
        fs::create_dir_all(legacy.config_dir.join("shell")).unwrap();
        fs::write(legacy.config_dir.join("config.jsonc"), "config").unwrap();
        fs::write(legacy.config_dir.join("shell/bash-hook.sh"), "bash hook").unwrap();
        fs::write(legacy.config_dir.join("shell/zsh-hook.zsh"), "zsh hook").unwrap();
        fs::write(
            temp.path().join(".bashrc"),
            "before\n# >>> miyu bash hook >>>\nsource '/legacy/bash-hook.sh'\n# <<< miyu bash hook <<<\nafter\n",
        )
        .unwrap();
        fs::write(
            temp.path().join(".zshrc"),
            "before\n# >>> miyu zsh hook >>>\nsource '/legacy/zsh-hook.zsh'\n# <<< miyu zsh hook <<<\nafter\n",
        )
        .unwrap();
        fs::write(legacy.data_dir.join("data.bin"), "data").unwrap();
        fs::write(legacy.cache_dir.join("cache.bin"), "cache").unwrap();
        fs::write(legacy.state_dir.join("state.db"), "state").unwrap();
        fs::write(legacy.documents_dir.join("report.md"), "report").unwrap();
        fs::write(
            legacy.data_dir.join("documents/shared.txt"),
            "same document",
        )
        .unwrap();
        fs::write(legacy.documents_dir.join("shared.txt"), "same document").unwrap();
        fs::write(legacy.data_dir.join("pictures/shared.png"), "same picture").unwrap();
        fs::write(legacy.pictures_dirs[0].join("shared.png"), "same picture").unwrap();
        fs::write(legacy.pictures_dirs[0].join("lower.png"), "lower").unwrap();
        fs::write(legacy.pictures_dirs[1].join("shared.png"), "same picture").unwrap();
        fs::write(legacy.pictures_dirs[1].join("upper.png"), "upper").unwrap();

        migrate_legacy_layout(&legacy, &next).unwrap();

        for source in [
            &legacy.config_dir,
            &legacy.data_dir,
            &legacy.cache_dir,
            &legacy.state_dir,
            &legacy.documents_dir,
            &legacy.pictures_dirs[0],
            &legacy.pictures_dirs[1],
        ] {
            assert!(
                !source.exists(),
                "legacy source remains: {}",
                source.display()
            );
        }
        assert_eq!(
            fs::read_to_string(next.config_dir.join("config.jsonc")).unwrap(),
            "config"
        );
        assert_eq!(
            fs::read_to_string(next.config_dir.join("shell/bash-hook.sh")).unwrap(),
            "bash hook"
        );
        assert_eq!(
            fs::read_to_string(next.config_dir.join("shell/zsh-hook.zsh")).unwrap(),
            "zsh hook"
        );
        let bashrc = fs::read_to_string(temp.path().join(".bashrc")).unwrap();
        assert!(bashrc.contains(
            next.config_dir
                .join("shell/bash-hook.sh")
                .to_string_lossy()
                .as_ref()
        ));
        assert!(!bashrc.contains("/legacy/bash-hook.sh"));
        let zshrc = fs::read_to_string(temp.path().join(".zshrc")).unwrap();
        assert!(zshrc.contains(
            next.config_dir
                .join("shell/zsh-hook.zsh")
                .to_string_lossy()
                .as_ref()
        ));
        assert!(!zshrc.contains("/legacy/zsh-hook.zsh"));
        assert_eq!(
            fs::read_to_string(next.data_dir.join("data.bin")).unwrap(),
            "data"
        );
        assert_eq!(
            fs::read_to_string(next.cache_dir.join("cache.bin")).unwrap(),
            "cache"
        );
        assert_eq!(
            fs::read_to_string(next.state_dir.join("state.db")).unwrap(),
            "state"
        );
        assert_eq!(
            fs::read_to_string(next.data_dir.join("documents/report.md")).unwrap(),
            "report"
        );
        assert_eq!(
            fs::read_to_string(next.data_dir.join("documents/shared.txt")).unwrap(),
            "same document"
        );
        assert_eq!(
            fs::read_to_string(next.data_dir.join("pictures/shared.png")).unwrap(),
            "same picture"
        );
        assert_eq!(
            fs::read_to_string(next.data_dir.join("pictures/lower.png")).unwrap(),
            "lower"
        );
        assert_eq!(
            fs::read_to_string(next.data_dir.join("pictures/upper.png")).unwrap(),
            "upper"
        );
        assert!(next.marker().exists());
    }

    #[test]
    fn unified_layout_cross_source_conflict_has_zero_migration_writes() {
        let temp = tempfile::tempdir().unwrap();
        let (legacy, next) = test_layouts(temp.path());
        fs::create_dir_all(&legacy.config_dir).unwrap();
        fs::create_dir_all(&legacy.pictures_dirs[0]).unwrap();
        fs::create_dir_all(&legacy.pictures_dirs[1]).unwrap();
        fs::write(legacy.config_dir.join("config.jsonc"), "must stay").unwrap();
        fs::write(legacy.pictures_dirs[0].join("conflict.png"), "lower").unwrap();
        fs::write(legacy.pictures_dirs[1].join("conflict.png"), "upper").unwrap();

        let error = migrate_legacy_layout(&legacy, &next).unwrap_err();

        assert!(error.to_string().contains("conflicting legacy entries"));
        assert_eq!(
            fs::read_to_string(legacy.config_dir.join("config.jsonc")).unwrap(),
            "must stay"
        );
        assert_eq!(
            fs::read_to_string(legacy.pictures_dirs[0].join("conflict.png")).unwrap(),
            "lower"
        );
        assert_eq!(
            fs::read_to_string(legacy.pictures_dirs[1].join("conflict.png")).unwrap(),
            "upper"
        );
        assert!(!next.root_dir.exists());
    }

    #[test]
    fn unified_layout_late_destination_conflict_does_not_move_earlier_sources() {
        let temp = tempfile::tempdir().unwrap();
        let (legacy, next) = test_layouts(temp.path());
        fs::create_dir_all(&legacy.config_dir).unwrap();
        fs::create_dir_all(&legacy.documents_dir).unwrap();
        fs::create_dir_all(next.data_dir.join("documents")).unwrap();
        fs::write(legacy.config_dir.join("config.jsonc"), "must stay").unwrap();
        fs::write(legacy.documents_dir.join("conflict.md"), "legacy").unwrap();
        fs::write(next.data_dir.join("documents/conflict.md"), "current").unwrap();

        let error = migrate_legacy_layout(&legacy, &next).unwrap_err();

        assert!(error.to_string().contains("conflicting entries"));
        assert!(legacy.config_dir.join("config.jsonc").exists());
        assert!(!next.config_dir.exists());
        assert_eq!(
            fs::read_to_string(legacy.documents_dir.join("conflict.md")).unwrap(),
            "legacy"
        );
        assert_eq!(
            fs::read_to_string(next.data_dir.join("documents/conflict.md")).unwrap(),
            "current"
        );
        assert!(!next.marker().exists());
    }

    #[test]
    fn interrupted_shell_hook_refresh_is_retried_before_marking_layout_complete() {
        let temp = tempfile::tempdir().unwrap();
        let (legacy, next) = test_layouts(temp.path());
        fs::create_dir_all(legacy.config_dir.join("shell")).unwrap();
        fs::write(legacy.config_dir.join("shell/bash-hook.sh"), "bash hook").unwrap();
        fs::write(
            temp.path().join(".bashrc"),
            "# >>> miyu bash hook >>>\nsource '/legacy/bash-hook.sh'\n",
        )
        .unwrap();

        let error = migrate_legacy_layout(&legacy, &next).unwrap_err();
        assert!(error.to_string().contains("refreshing shell hook paths"));
        assert!(!next.marker().exists());
        assert!(next.config_dir.join("shell/bash-hook.sh").exists());

        fs::write(
            temp.path().join(".bashrc"),
            "# >>> miyu bash hook >>>\nsource '/legacy/bash-hook.sh'\n# <<< miyu bash hook <<<\n",
        )
        .unwrap();
        migrate_legacy_layout(&legacy, &next).unwrap();

        let bashrc = fs::read_to_string(temp.path().join(".bashrc")).unwrap();
        assert!(bashrc.contains(
            next.config_dir
                .join("shell/bash-hook.sh")
                .to_string_lossy()
                .as_ref()
        ));
        assert!(!bashrc.contains("/legacy/bash-hook.sh"));
        assert!(next.marker().exists());
    }

    #[test]
    fn migration_rejects_relative_symbolic_links() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("legacy");
        let destination = temp.path().join("next");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(temp.path().join("outside")).unwrap();
        fs::write(temp.path().join("outside/value"), "value").unwrap();
        symlink("../outside/value", source.join("link")).unwrap();

        let error = migrate_entry(&source, &destination).unwrap_err();

        assert!(error.to_string().contains("relative symbolic link"));
        assert!(source.join("link").exists());
        assert!(!destination.exists());
    }

    #[test]
    fn migration_rejects_symlinked_destination_root_and_marker() {
        let temp = tempfile::tempdir().unwrap();
        let (legacy, next) = test_layouts(temp.path());
        fs::create_dir_all(&legacy.config_dir).unwrap();
        fs::write(legacy.config_dir.join("config.jsonc"), "config").unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();

        symlink(&outside, &next.root_dir).unwrap();
        let root_error = migrate_legacy_layout(&legacy, &next).unwrap_err();
        assert!(root_error.to_string().contains("symbolic-link directory"));
        fs::remove_file(&next.root_dir).unwrap();

        fs::create_dir_all(&next.root_dir).unwrap();
        let marker_target = temp.path().join("marker-target");
        fs::write(&marker_target, "1\n").unwrap();
        symlink(&marker_target, next.marker()).unwrap();
        let marker_error = migrate_legacy_layout(&legacy, &next).unwrap_err();
        assert!(marker_error.to_string().contains("layout marker"));
    }

    #[test]
    fn legacy_data_and_state_alias_is_migrated_once() {
        let temp = tempfile::tempdir().unwrap();
        let (mut legacy, next) = test_layouts(temp.path());
        legacy.state_dir = legacy.data_dir.clone();
        fs::create_dir_all(&legacy.data_dir).unwrap();
        fs::write(legacy.data_dir.join("shared.db"), "state and data").unwrap();

        migrate_legacy_layout(&legacy, &next).unwrap();

        assert_eq!(
            fs::read_to_string(next.data_dir.join("shared.db")).unwrap(),
            "state and data"
        );
        assert!(!legacy.data_dir.exists());
    }

    #[test]
    fn default_runtime_directory_keeps_the_legacy_name() {
        let runtime_root = Path::new("/run/user/1000");
        assert_eq!(
            runtime_dir_for(runtime_root, None),
            runtime_root.join("miyu")
        );
    }

    #[test]
    fn explicit_homes_get_stable_isolated_runtime_directories() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("runtime");
        let first = temp.path().join("homes/first");
        let second = temp.path().join("homes/second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();

        let first_runtime = runtime_dir_for(&runtime_root, Some(&first));
        assert_eq!(first_runtime, runtime_dir_for(&runtime_root, Some(&first)));
        assert_ne!(first_runtime, runtime_dir_for(&runtime_root, Some(&second)));
        assert!(first_runtime
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("miyu-"));
    }

    #[test]
    fn resource_path_remapping_includes_the_legacy_xdg_config_root() {
        let base = BaseDirs::new().unwrap();
        let root = base.home_dir().join(".miyu");
        let paths = MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("data/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("data/pictures"),
            fish_hook_file: base.config_dir().join("fish/conf.d/miyu.fish"),
            bash_hook_file: root.join("config/shell/bash-hook.sh"),
            zsh_hook_file: root.join("config/shell/zsh-hook.zsh"),
            scripts_dir: root.join("data/scripts"),
            system_scripts_dir: PathBuf::from("/usr/share/miyu/scripts"),
        };
        assert_eq!(
            paths.migrated_resource_path(&base.config_dir().join("miyu/prompts/team")),
            Some(root.join("data/prompts/team"))
        );
        assert_eq!(
            paths.migrated_resource_path(Path::new("prompts/../scripts/images")),
            Some(root.join("data/scripts/images"))
        );
    }

    #[test]
    fn runtime_hash_uses_a_normalized_home_path() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join("child")).unwrap();
        let equivalent = home.join("child/..");

        assert_eq!(
            runtime_dir_for(temp.path(), Some(&home)),
            runtime_dir_for(temp.path(), Some(&equivalent))
        );
    }

    #[test]
    fn legacy_layout_stays_put_while_the_core_lock_is_held() {
        let temp = tempfile::tempdir().unwrap();
        let (legacy, _) = test_layouts(temp.path());
        let runtime_dir = legacy.state_dir.join("miyu");
        fs::create_dir_all(&runtime_dir).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(runtime_dir.join("core.lock"))
            .unwrap();
        assert!(sys::flock_lock_ex(&lock, true).unwrap());

        assert!(legacy_daemon_is_running_at(&legacy, None));
        sys::flock_unlock(&lock);
        assert!(!legacy_daemon_is_running_at(&legacy, None));
    }

    #[test]
    fn legacy_layout_stays_put_while_the_starter_lock_is_held() {
        let temp = tempfile::tempdir().unwrap();
        let (legacy, _) = test_layouts(temp.path());
        let runtime_dir = legacy.state_dir.join("miyu");
        fs::create_dir_all(&runtime_dir).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(runtime_dir.join("starter.lock"))
            .unwrap();
        assert!(sys::flock_lock_ex(&lock, true).unwrap());

        assert!(legacy_daemon_is_running_at(&legacy, None));
        sys::flock_unlock(&lock);
        assert!(!legacy_daemon_is_running_at(&legacy, None));
    }

    #[test]
    fn resource_migration_defers_for_starters_except_inside_the_spawned_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_dir = temp.path().join("runtime");
        fs::create_dir_all(&runtime_dir).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(runtime_dir.join("starter.lock"))
            .unwrap();
        assert!(sys::flock_lock_ex(&lock, true).unwrap());

        assert!(daemon_is_running_at(&runtime_dir, false));
        assert!(!daemon_is_running_at(&runtime_dir, true));
    }

    #[test]
    fn resource_migration_holds_runtime_exclusion_through_commit() {
        let temp = tempfile::tempdir().unwrap();
        let (_, layout) = test_layouts(temp.path());
        fs::create_dir_all(layout.config_dir.join("skills/demo")).unwrap();
        fs::write(layout.config_dir.join("skills/demo/SKILL.md"), "skill").unwrap();
        let runtime_dir = layout.state_dir.join("miyu");
        fs::create_dir_all(&runtime_dir).unwrap();
        let starter = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(runtime_dir.join("starter.lock"))
            .unwrap();
        assert!(sys::flock_lock_ex(&starter, true).unwrap());

        assert!(!try_migrate_resource_layout(&layout, false).unwrap());
        assert!(layout.config_dir.join("skills/demo/SKILL.md").is_file());
        assert!(try_migrate_resource_layout(&layout, true).unwrap());
        assert!(layout.data_dir.join("skills/demo/SKILL.md").is_file());
        assert!(layout.resource_marker().is_file());
    }

    #[test]
    fn concurrent_client_waits_for_resource_migration_marker() {
        let temp = tempfile::tempdir().unwrap();
        let (_, layout) = test_layouts(temp.path());
        fs::create_dir_all(&layout.root_dir).unwrap();
        let lease = acquire_migration_lock(&layout.root_dir).unwrap();
        let concurrent_layout = layout.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            sender
                .send(try_migrate_resource_layout(&concurrent_layout, false))
                .unwrap();
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(receiver.try_recv().is_err());

        write_marker(&layout.resource_marker()).unwrap();
        drop(lease);
        assert!(receiver.recv().unwrap().unwrap());
        thread.join().unwrap();
    }
}
