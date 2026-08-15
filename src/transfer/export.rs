//! `miyu export`: pack a portable copy of this installation.

use super::manifest::{Entry, Manifest, Scope, MANIFEST_FORMAT_VERSION, MANIFEST_NAME};
use super::registry::{is_backup_name, unit_for, DataUnit, UnitKind, IGNORED_SUFFIXES, UNITS};
use crate::i18n::text as t;
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct ExportOptions {
    pub all: bool,
    pub index: bool,
    pub platforms: bool,
    pub no_secrets: bool,
    pub dry_run: bool,
    pub force: bool,
}

impl ExportOptions {
    fn includes(&self, unit: &DataUnit) -> bool {
        unit.included(self.all, self.index, self.platforms)
    }
}

/// One concrete file destined for the archive.
struct Planned {
    unit: &'static str,
    /// Where it lands in the archive, relative to `MIYU_HOME`.
    rel: String,
    /// Where to read it from — the live file, or a snapshot in a temp dir.
    source: PathBuf,
}

#[derive(Debug)]
pub struct ExportReport {
    pub archive: Option<PathBuf>,
    pub entries: usize,
    pub bytes: u64,
    pub secrets_included: bool,
    /// Uncompressed bytes per unit id, largest first — what `--dry-run` shows
    /// so the size of an archive is accountable rather than a single number.
    pub by_unit: Vec<(&'static str, u64)>,
}

pub fn export(paths: &MiyuPaths, output: &Path, options: &ExportOptions) -> Result<ExportReport> {
    let root = miyu_home(paths)?;
    if !options.dry_run && output.exists() && !options.force {
        bail!(
            "{}: {}",
            t(
                "output already exists; pass --force to overwrite",
                "输出文件已存在；如需覆盖请传 --force"
            ),
            output.display()
        );
    }

    // Snapshots outlive the planning step, so the temp dir must too.
    let snapshots = tempfile::tempdir().context("creating a snapshot directory")?;
    let mut planned = Vec::new();
    let mut schema_versions = BTreeMap::new();
    for unit in UNITS.iter().filter(|unit| options.includes(unit)) {
        plan_unit(
            &root,
            unit,
            snapshots.path(),
            &mut planned,
            &mut schema_versions,
        )?;
    }
    planned.sort_by(|left, right| left.rel.cmp(&right.rel));

    let redacted = if options.no_secrets {
        Some(redact_secrets(&root, snapshots.path(), &mut planned)?)
    } else {
        None
    };
    let _ = redacted;

    let mut entries = Vec::with_capacity(planned.len());
    let mut bytes = 0u64;
    let mut per_unit: BTreeMap<&'static str, u64> = BTreeMap::new();
    for item in &planned {
        let data = std::fs::read(&item.source)
            .with_context(|| format!("reading {}", item.source.display()))?;
        bytes += data.len() as u64;
        *per_unit.entry(item.unit).or_default() += data.len() as u64;
        entries.push(Entry {
            unit: item.unit.to_string(),
            path: item.rel.clone(),
            size: data.len() as u64,
            blake3: blake3::hash(&data).to_hex().to_string(),
        });
    }

    let manifest = Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        miyu_version: env!("CARGO_PKG_VERSION").to_string(),
        exported_at: chrono::Local::now().to_rfc3339(),
        config_version: crate::config::CURRENT_CONFIG_VERSION,
        schema_versions,
        scope: Scope {
            index: options.all || options.index,
            platforms: options.all || options.platforms,
        },
        secrets_included: !options.no_secrets,
        entries,
    };

    let mut by_unit: Vec<(&'static str, u64)> = per_unit.into_iter().collect();
    by_unit.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));

    if options.dry_run {
        return Ok(ExportReport {
            archive: None,
            entries: manifest.entries.len(),
            bytes,
            secrets_included: manifest.secrets_included,
            by_unit,
        });
    }

    write_archive(output, &manifest, &planned)?;
    // The archive carries plaintext credentials unless they were stripped, so
    // it must not be world-readable even for a moment.
    crate::sys::set_secure_file_permissions(output)
        .with_context(|| format!("restricting permissions on {}", output.display()))?;

    Ok(ExportReport {
        archive: Some(output.to_path_buf()),
        entries: manifest.entries.len(),
        bytes,
        secrets_included: manifest.secrets_included,
        by_unit,
    })
}

/// `MIYU_HOME` — the common ancestor of the config/data/state/cache roots.
pub fn miyu_home(paths: &MiyuPaths) -> Result<PathBuf> {
    paths
        .config_dir
        .parent()
        .map(Path::to_path_buf)
        .context("could not determine MIYU_HOME from the config directory")
}

fn plan_unit(
    root: &Path,
    unit: &'static DataUnit,
    snapshots: &Path,
    planned: &mut Vec<Planned>,
    schema_versions: &mut BTreeMap<String, i64>,
) -> Result<()> {
    for rel in expand(root, unit.rel) {
        let absolute = root.join(&rel);
        if !absolute.exists() {
            continue;
        }
        match unit.kind {
            UnitKind::File => planned.push(Planned {
                unit: unit.id,
                rel,
                source: absolute,
            }),
            UnitKind::Dir => {
                let mut files = Vec::new();
                collect_dir(&absolute, &rel, &mut files)?;
                for (rel, source) in files {
                    planned.push(Planned {
                        unit: unit.id,
                        rel,
                        source,
                    });
                }
            }
            UnitKind::Sqlite => {
                let snapshot = snapshots.join(rel.replace('/', "_"));
                let version = snapshot_sqlite(&absolute, &snapshot)?;
                schema_versions.insert(unit.id.to_string(), version);
                planned.push(Planned {
                    unit: unit.id,
                    rel,
                    source: snapshot,
                });
            }
        }
    }
    Ok(())
}

/// Resolves a `*` segment against the directories actually present.
fn expand(root: &Path, pattern: &str) -> Vec<String> {
    let mut results = vec![String::new()];
    for segment in pattern.split('/') {
        let mut next = Vec::new();
        for prefix in &results {
            if segment == "*" {
                let dir = if prefix.is_empty() {
                    root.to_path_buf()
                } else {
                    root.join(prefix)
                };
                let Ok(children) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for child in children.flatten() {
                    let Ok(name) = child.file_name().into_string() else {
                        continue;
                    };
                    next.push(join_rel(prefix, &name));
                }
            } else {
                next.push(join_rel(prefix, segment));
            }
        }
        results = next;
    }
    results.retain(|rel| !rel.is_empty());
    results
}

fn join_rel(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}/{segment}")
    }
}

fn collect_dir(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let children = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for child in children.flatten() {
        let Ok(name) = child.file_name().into_string() else {
            continue;
        };
        if skip_name(&name) {
            continue;
        }
        let path = child.path();
        let child_rel = join_rel(rel, &name);
        // A nested unit wins over the directory that contains it: this is how
        // `data/kb/semantic_index.db` stays out of a default export even
        // though `data/kb/files` travels.
        if let Some(owner) = unit_for(&child_rel) {
            if owner.rel != rel && owner.rel.len() > rel.len() {
                continue;
            }
        }
        if path.is_dir() {
            collect_dir(&path, &child_rel, out)?;
        } else if path.is_file() {
            out.push((child_rel, path));
        }
    }
    Ok(())
}

fn skip_name(name: &str) -> bool {
    is_backup_name(name) || IGNORED_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// Copies a database through SQLite rather than the filesystem.
///
/// The daemon keeps these open with a live WAL; `cp` can catch a torn page or
/// miss committed data still sitting in the log. `VACUUM INTO` writes a
/// consistent snapshot from a read transaction and compacts it on the way out.
fn snapshot_sqlite(source: &Path, destination: &Path) -> Result<i64> {
    let conn = rusqlite::Connection::open_with_flags(
        source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {} read-only", source.display()))?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    if destination.exists() {
        std::fs::remove_file(destination).ok();
    }
    conn.execute(
        "VACUUM INTO ?1",
        rusqlite::params![destination.to_string_lossy()],
    )
    .with_context(|| format!("snapshotting {}", source.display()))?;
    Ok(version)
}

/// Rewrites the config into the snapshot area with credentials blanked.
fn redact_secrets(root: &Path, snapshots: &Path, planned: &mut [Planned]) -> Result<usize> {
    let mut redacted = 0usize;
    for item in planned.iter_mut() {
        let Some(unit) = unit_for(&item.rel) else {
            continue;
        };
        if !unit.secret || !item.rel.ends_with("config.jsonc") {
            continue;
        }
        let raw = std::fs::read_to_string(root.join(&item.rel))
            .with_context(|| format!("reading {}", item.rel))?;
        let stripped = json_comments::StripComments::new(raw.as_bytes());
        let mut value: serde_json::Value =
            serde_json::from_reader(stripped).context("parsing config.jsonc for redaction")?;
        redacted += blank_secrets(&mut value);
        let destination = snapshots.join("config.redacted.jsonc");
        std::fs::write(&destination, serde_json::to_vec_pretty(&value)?)
            .context("writing the redacted config")?;
        item.source = destination;
    }
    Ok(redacted)
}

/// Blanks every credential-shaped value, wherever it sits in the tree.
///
/// Keyed on the field name rather than a fixed path list: plugins come and go,
/// and a new one carrying `api_key` should be covered the day it lands.
fn blank_secrets(value: &mut serde_json::Value) -> usize {
    const SECRET_KEYS: &[&str] = &["api_key", "api_keys", "access_token", "env"];
    let mut count = 0;
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let secret = SECRET_KEYS.contains(&key.as_str())
                    || key.ends_with("_api_key")
                    || key.ends_with("_api_keys")
                    || key.ends_with("_token");
                if secret {
                    count += 1;
                    *child = match child {
                        serde_json::Value::Array(_) => serde_json::Value::Array(Vec::new()),
                        serde_json::Value::Object(_) => {
                            serde_json::Value::Object(serde_json::Map::new())
                        }
                        _ => serde_json::Value::String(String::new()),
                    };
                } else {
                    count += blank_secrets(child);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                count += blank_secrets(item);
            }
        }
        _ => {}
    }
    count
}

fn write_archive(output: &Path, manifest: &Manifest, planned: &[Planned]) -> Result<()> {
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let file = File::create(output).with_context(|| format!("creating {}", output.display()))?;
    crate::sys::set_secure_file_permissions(output).ok();
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);

    let manifest_bytes = serde_json::to_vec_pretty(manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, MANIFEST_NAME, manifest_bytes.as_slice())
        .context("writing the manifest")?;

    for item in planned {
        let mut file = File::open(&item.source)
            .with_context(|| format!("opening {}", item.source.display()))?;
        let metadata = file.metadata()?;
        let mut header = tar::Header::new_gnu();
        header.set_size(metadata.len());
        header.set_mode(0o600);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("home/{}", item.rel), &mut file)
            .with_context(|| format!("adding {}", item.rel))?;
    }

    let encoder = builder.into_inner().context("finishing the archive")?;
    let mut file = encoder.finish().context("compressing the archive")?;
    file.flush()?;
    Ok(())
}
