//! 资源目录布局的迁移。
//!
//! 搬用户的数据目录，所以每一步都往最坏处设想：先预检
//! （`preflight_resource_entries` 查跨文件系统、路径重叠、符号链接指向），再写
//! 日志（`write_resource_journal`），再原子移动。中途崩掉靠
//! `recover_resource_migration` 从日志接着走。
//!
//! `ResourceDaemonGuard` 保证迁移期间没有 daemon 在跑——一边搬一边写必然坏。

use crate::paths::*;

pub(crate) const RESOURCE_LAYOUT_MARKER: &str = ".resource-layout-v1";

pub(crate) const RESOURCE_MIGRATION_JOURNAL: &str = ".resource-layout-v1.journal.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ResourceMigrationEntry {
    pub(crate) source: PathBuf,
    pub(crate) destination: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ResourceMigrationJournal {
    pub(crate) entries: Vec<ResourceMigrationEntry>,
    pub(crate) moved: usize,
    #[serde(default)]
    pub(crate) pending: Option<usize>,
}

pub(crate) fn resource_layout_entries(layout: &Layout) -> Vec<ResourceMigrationEntry> {
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

pub(crate) fn resource_runtime_dir(layout: &Layout) -> PathBuf {
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

pub(crate) fn normalize_resource_relative_path(path: &Path) -> Option<PathBuf> {
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

pub(crate) fn resource_layout_marker_exists(layout: &Layout) -> Result<bool> {
    marker_exists_at(&layout.resource_marker(), "resource layout marker")
}

pub(crate) fn migrate_resource_layout(layout: &Layout) -> Result<()> {
    if !try_migrate_resource_layout(layout, false)? {
        bail!("Miyu resource migration is deferred while another daemon or starter is active");
    }
    Ok(())
}

pub(crate) fn try_migrate_resource_layout(
    layout: &Layout,
    current_process_is_daemon: bool,
) -> Result<bool> {
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
    run_journaled_moves(&layout.resource_journal(), entries)?;
    write_marker(&layout.resource_marker())?;
    remove_resource_journal_if_present(layout)?;
    Ok(true)
}

/// 逐条原子移动,每一步先落日志;中途失败就按日志回滚,回滚也失败则两个
/// 错误一起报。资源迁移与家目录迁移共用。
pub(crate) fn run_journaled_moves(
    journal_path: &Path,
    entries: Vec<ResourceMigrationEntry>,
) -> Result<()> {
    let mut journal = ResourceMigrationJournal {
        entries,
        moved: 0,
        pending: None,
    };
    write_journal_at(journal_path, &journal)?;
    while journal.moved < journal.entries.len() {
        let index = journal.moved;
        let entry = journal.entries[index].clone();
        journal.pending = Some(index);
        write_journal_at(journal_path, &journal)?;
        if let Err(error) = atomic_resource_move(&entry.source, &entry.destination) {
            let recovery = recover_migration_at(journal_path);
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
        write_journal_at(journal_path, &journal)?;
    }
    Ok(())
}

pub(crate) struct ResourceDaemonGuard {
    pub(crate) _starter: Option<File>,
    pub(crate) _core: File,
}

pub(crate) fn try_acquire_resource_daemon_guard(
    layout: &Layout,
    current_process_is_daemon: bool,
) -> Result<Option<ResourceDaemonGuard>> {
    let runtime_dir = resource_runtime_dir(layout);
    ensure_private_dir(&runtime_dir)?;
    let starter_path = runtime_dir.join("starter.lock");
    let starter_already_held = current_process_is_daemon && runtime_lock_is_held(&starter_path);
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
    if std::os::unix::net::UnixStream::connect(runtime_dir.join("core.sock")).is_ok() {
        return Ok(None);
    }
    Ok(Some(ResourceDaemonGuard {
        _starter: starter,
        _core: core,
    }))
}

pub(crate) fn preflight_resource_entries(
    layout: &Layout,
    entries: &[ResourceMigrationEntry],
) -> Result<()> {
    preflight_entries(&layout.data_dir, entries)
}

/// 预检:源不能是符号链接、目标不能已存在、目标必须在 `root` 之下且祖先
/// 不是链接、同一文件系统、目标之间不互相嵌套。失败零写入。
pub(crate) fn preflight_entries(root: &Path, entries: &[ResourceMigrationEntry]) -> Result<()> {
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
        ensure_destination_ancestors_under(root, &entry.destination)?;
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

pub(crate) fn ensure_destination_ancestors(layout: &Layout, destination: &Path) -> Result<()> {
    ensure_destination_ancestors_under(&layout.data_dir, destination)
}

pub(crate) fn ensure_destination_ancestors_under(root: &Path, destination: &Path) -> Result<()> {
    let relative = destination.strip_prefix(root).with_context(|| {
        format!(
            "resource destination escapes {}: {}",
            root.display(),
            destination.display()
        )
    })?;
    let mut current = root.to_path_buf();
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
pub(crate) fn ensure_resource_same_filesystem(source: &Path, destination: &Path) -> Result<()> {
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
pub(crate) fn ensure_resource_same_filesystem(_source: &Path, _destination: &Path) -> Result<()> {
    Ok(())
}

pub(crate) fn ensure_resource_targets_do_not_overlap(
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

pub(crate) fn atomic_resource_move(source: &Path, destination: &Path) -> Result<()> {
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

pub(crate) fn write_resource_journal(
    layout: &Layout,
    journal: &ResourceMigrationJournal,
) -> Result<()> {
    write_journal_at(&layout.resource_journal(), journal)
}

pub(crate) fn write_journal_at(path: &Path, journal: &ResourceMigrationJournal) -> Result<()> {
    let path = path.to_path_buf();
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    serde_json::to_writer_pretty(&mut file, journal)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("installing resource migration journal {}", path.display()))?;
    sync_parent(&path)
}

pub(crate) fn recover_resource_migration(layout: &Layout) -> Result<()> {
    recover_migration_at(&layout.resource_journal())
}

/// 按日志接着走或回滚:pending 那条看两头哪边存在定进退,已搬的逆序搬回。
pub(crate) fn recover_migration_at(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
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
        write_journal_at(&path, &journal)?;
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
        write_journal_at(&path, &journal)?;
    }
    remove_journal_if_present_at(&path)
}

pub(crate) fn remove_resource_journal_if_present(layout: &Layout) -> Result<()> {
    remove_journal_if_present_at(&layout.resource_journal())
}

pub(crate) fn remove_journal_if_present_at(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
