use crate::config::{persona_scope_name, AppConfig};
use crate::paths::MiyuPaths;
use crate::sys;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use yaml_rust2::scanner::{Scanner, Token, TokenType};
use yaml_rust2::{Yaml, YamlLoader};

/// Skills compiled into the binary: (name, raw SKILL.md). A user skill of
/// the same name in the persona/global directories overrides the built-in.
const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("skill-creator", include_str!("skills/skill-creator.md")),
    (
        "linux-input-method-diagnose",
        include_str!("skills/linux-input-method-diagnose.md"),
    ),
    (
        "linux-game-compatibility",
        include_str!("skills/linux-game-compatibility.md"),
    ),
];
const DRAFT_MANIFEST: &str = "draft.json";
const DRAFT_PACKAGE_DIR: &str = "package";
const DRAFT_VERSION: u32 = 1;
const DRAFT_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const PUBLISHED_DRAFT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_SKILL_FILE_BYTES: u64 = 256 * 1024;
const MAX_SKILL_PACKAGE_FILES: usize = 512;
const MAX_SKILL_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SKILL_PACKAGE_DIRS: usize = 128;
const MAX_SKILL_PACKAGE_DEPTH: usize = 16;
const MAX_DRAFT_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_YAML_TOKENS: usize = 4_096;
const PUBLISH_LOCK_FILE: &str = ".publish.lock";
const MAX_SKILL_CATALOG_ENTRIES: usize = 256;
const MAX_SKILL_ROOT_DIRECTORIES: usize = 1_024;
const MAX_SKILL_RESOURCE_ENTRIES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillSource {
    Persona,
    Global,
    BuiltIn,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Persona => "persona",
            Self::Global => "global",
            Self::BuiltIn => "built_in",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SkillEntry {
    pub metadata: SkillMetadata,
    pub source: SkillSource,
    pub directory: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct LoadedSkill {
    pub metadata: SkillMetadata,
    pub body: String,
    pub source: SkillSource,
    pub base_dir: Option<PathBuf>,
    pub files: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Global,
    Persona,
}

impl SkillScope {
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("global").trim() {
            "" | "global" => Ok(Self::Global),
            "persona" => Ok(Self::Persona),
            other => bail!("invalid skill scope: {other}; expected global or persona"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Persona => "persona",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DraftKind {
    Create,
    Update,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DraftManifest {
    version: u32,
    id: String,
    name: String,
    scope: SkillScope,
    persona_scope: Option<String>,
    kind: DraftKind,
    base_revision: Option<String>,
    created_at: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SkillDraft {
    pub id: String,
    pub name: String,
    pub scope: String,
    pub persona_scope: Option<String>,
    pub kind: String,
    pub skill_dir: String,
    pub skill_file: String,
    pub base_revision: Option<String>,
    pub created_at: u64,
    pub last_modified_at: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublishedSkill {
    pub name: String,
    pub scope: String,
    pub persona_scope: Option<String>,
    pub path: String,
    pub revision: String,
    pub operation: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeletedSkill {
    pub name: String,
    pub scope: String,
    pub persona_scope: Option<String>,
    pub path: String,
}

pub fn discover(config: &AppConfig, paths: &MiyuPaths) -> Result<Vec<SkillEntry>> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for (root, source) in skill_roots(config, paths) {
        for directory in sorted_skill_directories(&root)? {
            if directory.join(".disabled").exists() {
                continue;
            }
            let skill_file = directory.join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let raw = match read_skill_file(&skill_file) {
                Ok(raw) => raw,
                Err(error) => {
                    tracing::warn!(path = %skill_file.display(), error = %error, "skipping unreadable skill");
                    continue;
                }
            };
            let directory_name = directory
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let metadata = match parse_skill_metadata(&raw, Some(directory_name)) {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::warn!(path = %skill_file.display(), error = %error, "skipping invalid skill");
                    continue;
                }
            };
            if seen.insert(metadata.name.clone()) {
                if entries.len() >= MAX_SKILL_CATALOG_ENTRIES.saturating_sub(1) {
                    bail!("skill catalog exceeds the {MAX_SKILL_CATALOG_ENTRIES} entry limit");
                }
                entries.push(SkillEntry {
                    metadata,
                    source,
                    directory: Some(directory),
                });
            }
        }
    }
    for (name, raw) in BUILTIN_SKILLS {
        if !seen.contains(*name) {
            entries.push(SkillEntry {
                metadata: parse_skill_metadata(raw, Some(name))?,
                source: SkillSource::BuiltIn,
                directory: None,
            });
        }
    }
    Ok(entries)
}

pub fn catalog_fingerprint(config: &AppConfig, paths: &MiyuPaths) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    for (root, source) in skill_roots(config, paths) {
        hasher.update(source.as_str().as_bytes());
        hasher.update(root.as_os_str().as_encoded_bytes());
        for directory in sorted_skill_directories(&root)? {
            hasher.update(directory.as_os_str().as_encoded_bytes());
            hash_metadata(&mut hasher, &directory.join(".disabled"))?;
            hash_metadata(&mut hasher, &directory.join("SKILL.md"))?;
        }
    }
    for (_, raw) in BUILTIN_SKILLS {
        hasher.update(raw.as_bytes());
    }
    Ok(*hasher.finalize().as_bytes())
}

pub fn load(name: &str, config: &AppConfig, paths: &MiyuPaths) -> Result<LoadedSkill> {
    let name = name.trim();
    if name.is_empty() {
        bail!("skill name is required");
    }
    let entry = discover(config, paths)?
        .into_iter()
        .find(|entry| entry.metadata.name == name)
        .ok_or_else(|| anyhow::anyhow!("skill not found: {name}"))?;
    if let Some(directory) = entry.directory {
        let raw = read_skill_file(&directory.join("SKILL.md"))?;
        let (metadata, body) = parse_skill_document(&raw, Some(name))?;
        let mut files = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "SKILL.md" || name.to_string_lossy().starts_with('.') {
                continue;
            }
            if files.len() >= MAX_SKILL_RESOURCE_ENTRIES {
                bail!(
                    "skill resource manifest exceeds the {MAX_SKILL_RESOURCE_ENTRIES} entry limit"
                );
            }
            files.push(entry.path());
        }
        files.sort();
        return Ok(LoadedSkill {
            metadata,
            body,
            source: entry.source,
            base_dir: Some(directory),
            files,
        });
    }
    let raw = BUILTIN_SKILLS
        .iter()
        .find(|(builtin_name, _)| *builtin_name == name)
        .map(|(_, raw)| *raw)
        .with_context(|| format!("skill not found: {name}"))?;
    let (metadata, body) = parse_skill_document(raw, Some(name))?;
    Ok(LoadedSkill {
        metadata,
        body,
        source: SkillSource::BuiltIn,
        base_dir: None,
        files: Vec::new(),
    })
}

pub fn create_draft(
    config: &AppConfig,
    paths: &MiyuPaths,
    name: &str,
    description: &str,
    scope: SkillScope,
) -> Result<SkillDraft> {
    prune_expired_drafts(paths)?;
    validate_skill_name(name)?;
    validate_description(description)?;
    let persona_scope = persona_scope(config, scope);
    let target = target_path(paths, name, scope, persona_scope.as_deref())?;
    if target.exists() {
        bail!("skill already exists in {} scope: {name}", scope.as_str());
    }
    let manifest = new_manifest(name, scope, persona_scope, DraftKind::Create, None);
    let package = create_empty_draft(paths, &manifest)?;
    let result = (|| {
        let skill_dir = package.join(name);
        fs::create_dir(&skill_dir)?;
        write_private_file(
            &skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {}\n---\n\n# {name}\n\n## Workflow\n\nDescribe the reusable workflow here.\n",
                serde_json::to_string(description.trim())?
            )
            .as_bytes(),
        )?;
        write_draft_manifest(paths, &manifest)?;
        draft_public(paths, &manifest)
    })();
    cleanup_failed_draft(paths, &manifest, result)
}

pub fn update_draft(
    config: &AppConfig,
    paths: &MiyuPaths,
    name: &str,
    scope: SkillScope,
) -> Result<SkillDraft> {
    prune_expired_drafts(paths)?;
    validate_skill_name(name)?;
    let persona_scope = persona_scope(config, scope);
    let source = target_path(paths, name, scope, persona_scope.as_deref())?;
    if !source.join("SKILL.md").is_file() {
        bail!("skill not found in {} scope: {name}", scope.as_str());
    }
    let _lease = acquire_publish_lock(paths)?;
    ensure_directory_chain(&paths.skills_dir, &source)?;
    validate_skill_package(&source, name)?;
    let revision_before = skill_revision(&source)?;
    let manifest = new_manifest(
        name,
        scope,
        persona_scope,
        DraftKind::Update,
        Some(revision_before.clone()),
    );
    let package = create_empty_draft(paths, &manifest)?;
    let result = (|| {
        copy_tree(&source, &package.join(name))?;
        let revision_after = skill_revision(&source)?;
        if revision_after != revision_before {
            bail!("skill changed while its update draft was being created; retry");
        }
        validate_skill_package(&package.join(name), name)?;
        write_draft_manifest(paths, &manifest)?;
        draft_public(paths, &manifest)
    })();
    cleanup_failed_draft(paths, &manifest, result)
}

pub fn publish_draft(paths: &MiyuPaths, draft_id: &str) -> Result<PublishedSkill> {
    validate_draft_id(draft_id)?;
    let _lease = acquire_publish_lock(paths)?;
    prune_expired_drafts_unlocked(paths)?;
    let manifest = read_manifest(paths, draft_id)?;
    let draft_root = paths.skill_drafts_dir().join(draft_id);
    let source = draft_root.join(DRAFT_PACKAGE_DIR).join(&manifest.name);
    ensure_directory_chain(&draft_root, &source)?;
    let target = target_path(
        paths,
        &manifest.name,
        manifest.scope,
        manifest.persona_scope.as_deref(),
    )?;
    let parent = target.parent().context("skill target has no parent")?;
    create_private_dir(&paths.skills_dir)?;
    create_private_directory_chain(&paths.skills_dir, parent)?;
    let staged = parent.join(format!(
        ".miyu-skill-stage-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let source_revision_before = skill_revision(&source)?;
    copy_tree(&source, &staged)?;
    let source_revision_after = skill_revision(&source)?;
    if source_revision_after != source_revision_before {
        bail!("skill draft changed while it was being published; retry");
    }
    let mut staged_guard = StagedDirectory::new(staged.clone());
    validate_skill_package(&staged, &manifest.name)?;
    let revision = skill_revision(&staged)?;
    if skill_revision(&source)? != source_revision_after {
        bail!("skill draft changed before installation; retry");
    }

    match manifest.kind {
        DraftKind::Create => {
            if target.exists() {
                bail!(
                    "skill already exists; create never overwrites: {}",
                    manifest.name
                );
            }
            install_new_skill(&staged, &target).with_context(|| {
                format!(
                    "publishing skill from {} to {}",
                    staged.display(),
                    target.display()
                )
            })?;
            if let Err(error) = File::open(parent).and_then(|directory| directory.sync_all()) {
                tracing::warn!(path = %parent.display(), error = %error, "failed to sync published skill directory");
            }
            staged_guard.disarm();
        }
        DraftKind::Update => {
            if !target.is_dir() {
                bail!("skill disappeared before update: {}", manifest.name);
            }
            ensure_directory_chain(&paths.skills_dir, &target)?;
            validate_skill_package(&target, &manifest.name)?;
            let expected = manifest
                .base_revision
                .as_deref()
                .context("update draft is missing its base revision")?;
            let current = skill_revision(&target)?;
            if current != expected {
                bail!(
                    "skill changed after the update draft was created; create a new update draft"
                );
            }
            install_updated_skill(&staged, &target, &current, &mut staged_guard)?;
        }
    }
    let archived_draft = paths.skill_drafts_dir().join(format!(
        ".published-{}-{:016x}",
        manifest.id,
        rand::random::<u64>()
    ));
    if let Err(error) = fs::rename(&draft_root, &archived_draft) {
        tracing::warn!(path = %draft_root.display(), error = %error, "failed to archive published skill draft");
    } else if let Err(error) = File::open(paths.skill_drafts_dir()).and_then(|dir| dir.sync_all()) {
        tracing::warn!(path = %archived_draft.display(), error = %error, "failed to sync published skill draft archive");
    }
    Ok(PublishedSkill {
        name: manifest.name,
        scope: manifest.scope.as_str().to_string(),
        persona_scope: manifest.persona_scope,
        path: target.display().to_string(),
        revision,
        operation: match manifest.kind {
            DraftKind::Create => "create",
            DraftKind::Update => "update",
        }
        .to_string(),
    })
}

pub fn delete_skill(
    config: &AppConfig,
    paths: &MiyuPaths,
    name: &str,
    scope: SkillScope,
) -> Result<DeletedSkill> {
    validate_skill_name(name)?;
    let persona_scope = persona_scope(config, scope);
    let target = target_path(paths, name, scope, persona_scope.as_deref())?;
    let _lease = acquire_publish_lock(paths)?;
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("skill path is unsafe: {}", target.display())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("skill not found in {} scope: {name}", scope.as_str())
        }
        Err(error) => return Err(error.into()),
    }
    ensure_directory_chain(&paths.skills_dir, &target)?;
    validate_skill_package(&target, name)?;
    fs::remove_dir_all(&target)?;
    if let Some(parent) = target.parent() {
        if let Err(error) = File::open(parent).and_then(|directory| directory.sync_all()) {
            tracing::warn!(path = %parent.display(), error = %error, "failed to sync deleted skill directory");
        }
    }
    Ok(DeletedSkill {
        name: name.to_string(),
        scope: scope.as_str().to_string(),
        persona_scope,
        path: target.display().to_string(),
    })
}

pub fn list_drafts(paths: &MiyuPaths) -> Result<Vec<SkillDraft>> {
    prune_expired_drafts(paths)?;
    let root = paths.skill_drafts_dir();
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut drafts = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if id.starts_with('.') {
            continue;
        }
        match read_manifest(paths, &id).and_then(|manifest| draft_public(paths, &manifest)) {
            Ok(draft) => drafts.push(draft),
            Err(error) => {
                tracing::warn!(draft_id = id, error = %error, "skipping invalid skill draft")
            }
        }
    }
    drafts.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then(left.id.cmp(&right.id))
    });
    Ok(drafts)
}

pub fn prune_expired_drafts(paths: &MiyuPaths) -> Result<usize> {
    let _lease = acquire_publish_lock(paths)?;
    prune_expired_drafts_unlocked(paths)
}

fn prune_expired_drafts_unlocked(paths: &MiyuPaths) -> Result<usize> {
    let root = paths.skill_drafts_dir();
    if !root.is_dir() {
        return Ok(0);
    }
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let published_archive = entry
            .file_name()
            .to_string_lossy()
            .starts_with(".published-");
        let inspection = match inspect_latest_modified(&entry.path()) {
            Ok(inspection) => inspection,
            Err(error) => {
                tracing::warn!(path = %entry.path().display(), error = %error, "failed to inspect skill draft age");
                continue;
            }
        };
        let modified = match inspection {
            DraftInspection::Valid(modified) => modified,
            DraftInspection::Invalid => {
                let error = anyhow::anyhow!("draft exceeds inspection limits");
                tracing::warn!(path = %entry.path().display(), error = %error, "removing invalid skill draft");
                match fs::remove_dir_all(entry.path()) {
                    Ok(()) => removed += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                continue;
            }
        };
        let age = now.duration_since(modified).unwrap_or_default();
        let retention = if published_archive {
            PUBLISHED_DRAFT_RETENTION
        } else {
            DRAFT_RETENTION
        };
        if age >= retention {
            tracing::info!(path = %entry.path().display(), "removing expired skill draft");
            match fs::remove_dir_all(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(removed)
}

pub fn is_generated_skill(raw: &str) -> bool {
    parse_skill_metadata(raw, None)
        .ok()
        .and_then(|metadata| metadata.metadata.get("miyu.generated").cloned())
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        || raw.contains("generated_by: miyu")
        || raw.contains("Auto-learned method from assistant conversation")
        || raw.contains("Auto-learned method from Miyu conversation")
}

pub fn parse_skill_metadata(raw: &str, expected_name: Option<&str>) -> Result<SkillMetadata> {
    parse_skill_document(raw, expected_name).map(|(metadata, _)| metadata)
}

fn parse_skill_document(raw: &str, expected_name: Option<&str>) -> Result<(SkillMetadata, String)> {
    let (frontmatter, body) = split_frontmatter(raw)?;
    validate_frontmatter_tokens(&frontmatter)?;
    let documents =
        YamlLoader::load_from_str(&frontmatter).context("parsing skill YAML frontmatter")?;
    if documents.len() != 1 {
        bail!("skill frontmatter must contain exactly one YAML document");
    }
    let mapping = documents[0]
        .as_hash()
        .context("skill frontmatter root must be a mapping")?;
    let name = required_yaml_string(mapping, "name")?;
    validate_skill_name(&name)?;
    if let Some(expected) = expected_name {
        if name != expected {
            bail!("skill name '{name}' does not match directory '{expected}'");
        }
    }
    let description = required_yaml_string(mapping, "description")?;
    validate_description(&description)?;
    let license = optional_yaml_string(mapping, "license")?;
    let compatibility = optional_yaml_string(mapping, "compatibility")?;
    if compatibility
        .as_ref()
        .is_some_and(|value| !(1..=500).contains(&value.chars().count()))
    {
        bail!("skill compatibility must be 1-500 characters");
    }
    let allowed_tools = optional_yaml_string(mapping, "allowed-tools")?;
    let metadata = yaml_string_map(mapping, "metadata")?;
    Ok((
        SkillMetadata {
            name,
            description,
            license,
            compatibility,
            metadata,
            allowed_tools,
        },
        body,
    ))
}

fn validate_frontmatter_tokens(frontmatter: &str) -> Result<()> {
    for (index, Token(_, token)) in Scanner::new(frontmatter.chars()).enumerate() {
        if index >= MAX_YAML_TOKENS {
            bail!("skill frontmatter exceeds the YAML token limit");
        }
        if matches!(token, TokenType::Alias(_) | TokenType::Anchor(_)) {
            bail!("skill frontmatter may not use YAML anchors or aliases");
        }
    }
    Ok(())
}

fn split_frontmatter(raw: &str) -> Result<(String, String)> {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.lines();
    if lines.next() != Some("---") {
        bail!("SKILL.md must begin with YAML frontmatter");
    }
    let mut frontmatter = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line == "---" {
            closed = true;
            break;
        }
        frontmatter.push(line);
    }
    if !closed {
        bail!("SKILL.md frontmatter is missing its closing ---");
    }
    Ok((frontmatter.join("\n"), lines.collect::<Vec<_>>().join("\n")))
}

fn required_yaml_string(mapping: &yaml_rust2::yaml::Hash, key: &str) -> Result<String> {
    optional_yaml_string(mapping, key)?.ok_or_else(|| anyhow::anyhow!("skill {key} is required"))
}

fn optional_yaml_string(mapping: &yaml_rust2::yaml::Hash, key: &str) -> Result<Option<String>> {
    let Some(value) = mapping.get(&Yaml::String(key.to_string())) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("skill {key} must be a string"))?
        .trim()
        .to_string();
    if value.is_empty() {
        bail!("skill {key} must not be empty");
    }
    Ok(Some(value))
}

fn yaml_string_map(
    mapping: &yaml_rust2::yaml::Hash,
    key: &str,
) -> Result<BTreeMap<String, String>> {
    let Some(value) = mapping.get(&Yaml::String(key.to_string())) else {
        return Ok(BTreeMap::new());
    };
    let values = value
        .as_hash()
        .ok_or_else(|| anyhow::anyhow!("skill {key} must be a string-to-string mapping"))?;
    let mut result = BTreeMap::new();
    for (name, value) in values {
        let name = name
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("skill {key} keys must be strings"))?;
        let value = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("skill {key} values must be strings"))?;
        result.insert(name.to_string(), value.to_string());
    }
    Ok(result)
}

fn validate_skill_name(name: &str) -> Result<()> {
    let len = name.chars().count();
    if !(1..=64).contains(&len) || name.starts_with('-') || name.ends_with('-') {
        bail!("skill name must be 1-64 lowercase ASCII letters, digits, or single hyphens");
    }
    let mut previous_hyphen = false;
    for character in name.chars() {
        let valid =
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-';
        if !valid || character == '-' && previous_hyphen {
            bail!("skill name must be 1-64 lowercase ASCII letters, digits, or single hyphens");
        }
        previous_hyphen = character == '-';
    }
    Ok(())
}

fn validate_description(description: &str) -> Result<()> {
    if !(1..=1024).contains(&description.trim().chars().count()) {
        bail!("skill description must be 1-1024 characters");
    }
    Ok(())
}

fn read_skill_file(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("SKILL.md must be a regular file: {}", path.display());
    }
    if metadata.len() > MAX_SKILL_FILE_BYTES {
        bail!("SKILL.md exceeds the {MAX_SKILL_FILE_BYTES} byte limit");
    }
    fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn skill_roots(config: &AppConfig, paths: &MiyuPaths) -> Vec<(PathBuf, SkillSource)> {
    vec![
        (
            config.active_persona_skills_dir(paths),
            SkillSource::Persona,
        ),
        (paths.skills_dir.clone(), SkillSource::Global),
    ]
}

fn sorted_skill_directories(root: &Path) -> Result<Vec<PathBuf>> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        bail!("skill root must not be a symbolic link: {}", root.display());
    }
    if !metadata.is_dir() {
        bail!("skill root is not a directory: {}", root.display());
    }
    let mut directories = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
            if directories.len() >= MAX_SKILL_ROOT_DIRECTORIES {
                bail!("skill root exceeds the {MAX_SKILL_ROOT_DIRECTORIES} directory-entry limit");
            }
            directories.push(entry.path());
        }
    }
    directories.sort();
    Ok(directories)
}

fn hash_metadata(hasher: &mut blake3::Hasher, path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            hasher.update(&[1]);
            hasher.update(&metadata.len().to_le_bytes());
            if let Ok(modified) = metadata.modified().and_then(|time| {
                time.duration_since(UNIX_EPOCH)
                    .map_err(std::io::Error::other)
            }) {
                hasher.update(&modified.as_secs().to_le_bytes());
                hasher.update(&modified.subsec_nanos().to_le_bytes());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                hasher.update(&metadata.ino().to_le_bytes());
                hasher.update(&metadata.ctime().to_le_bytes());
                hasher.update(&metadata.ctime_nsec().to_le_bytes());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hasher.update(&[0]);
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn persona_scope(config: &AppConfig, scope: SkillScope) -> Option<String> {
    (scope == SkillScope::Persona).then(|| config.active_persona_scope())
}

fn target_path(
    paths: &MiyuPaths,
    name: &str,
    scope: SkillScope,
    persona: Option<&str>,
) -> Result<PathBuf> {
    validate_skill_name(name)?;
    match scope {
        SkillScope::Global => {
            if persona.is_some() {
                bail!("global skill drafts may not contain a persona scope");
            }
            if name == "personas" {
                bail!("personas is reserved for persona-scoped skills");
            }
            Ok(paths.skills_dir.join(name))
        }
        SkillScope::Persona => {
            let persona = persona.context("persona skill draft is missing its persona scope")?;
            validate_persona_scope(persona)?;
            Ok(paths.skills_dir.join("personas").join(persona).join(name))
        }
    }
}

fn validate_persona_scope(scope: &str) -> Result<()> {
    if scope.is_empty()
        || scope.len() > 64
        || scope == "."
        || scope == ".."
        || scope != persona_scope_name(scope)
        || !scope
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid persona skill scope");
    }
    Ok(())
}

fn new_manifest(
    name: &str,
    scope: SkillScope,
    persona_scope: Option<String>,
    kind: DraftKind,
    base_revision: Option<String>,
) -> DraftManifest {
    DraftManifest {
        version: DRAFT_VERSION,
        id: format!("draft-{:032x}", rand::random::<u128>()),
        name: name.to_string(),
        scope,
        persona_scope,
        kind,
        base_revision,
        created_at: unix_time(SystemTime::now()),
    }
}

fn create_empty_draft(paths: &MiyuPaths, manifest: &DraftManifest) -> Result<PathBuf> {
    let root = paths.skill_drafts_dir();
    create_private_dir(&root)?;
    let draft = root.join(&manifest.id);
    fs::create_dir(&draft)?;
    let result = (|| {
        secure_directory(&draft)?;
        let package = draft.join(DRAFT_PACKAGE_DIR);
        fs::create_dir(&package)?;
        secure_directory(&package)?;
        Ok(package)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&draft);
    }
    result
}

fn write_draft_manifest(paths: &MiyuPaths, manifest: &DraftManifest) -> Result<()> {
    let draft = paths.skill_drafts_dir().join(&manifest.id);
    write_private_file(
        &draft.join(DRAFT_MANIFEST),
        format!("{}\n", serde_json::to_string_pretty(manifest)?).as_bytes(),
    )
}

fn cleanup_failed_draft<T>(
    paths: &MiyuPaths,
    manifest: &DraftManifest,
    result: Result<T>,
) -> Result<T> {
    if result.is_err() {
        let _ = fs::remove_dir_all(paths.skill_drafts_dir().join(&manifest.id));
    }
    result
}

fn draft_public(paths: &MiyuPaths, manifest: &DraftManifest) -> Result<SkillDraft> {
    let skill_dir = paths
        .skill_drafts_dir()
        .join(&manifest.id)
        .join(DRAFT_PACKAGE_DIR)
        .join(&manifest.name);
    Ok(SkillDraft {
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        scope: manifest.scope.as_str().to_string(),
        persona_scope: manifest.persona_scope.clone(),
        kind: match manifest.kind {
            DraftKind::Create => "create",
            DraftKind::Update => "update",
        }
        .to_string(),
        skill_file: skill_dir.join("SKILL.md").display().to_string(),
        skill_dir: skill_dir.display().to_string(),
        base_revision: manifest.base_revision.clone(),
        created_at: manifest.created_at,
        last_modified_at: unix_time(latest_modified(&skill_dir).unwrap_or(UNIX_EPOCH)),
    })
}

fn read_manifest(paths: &MiyuPaths, draft_id: &str) -> Result<DraftManifest> {
    validate_draft_id(draft_id)?;
    let draft = paths.skill_drafts_dir().join(draft_id);
    let metadata = fs::symlink_metadata(&draft)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("skill draft root must be a regular directory");
    }
    let path = draft.join(DRAFT_MANIFEST);
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("skill draft manifest must be a regular file");
    }
    if metadata.len() > MAX_DRAFT_MANIFEST_BYTES {
        bail!("skill draft manifest exceeds its size limit");
    }
    let manifest: DraftManifest = serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    if manifest.version != DRAFT_VERSION || manifest.id != draft_id {
        bail!("unsupported or mismatched skill draft manifest");
    }
    validate_skill_name(&manifest.name)?;
    target_path(
        paths,
        &manifest.name,
        manifest.scope,
        manifest.persona_scope.as_deref(),
    )?;
    match manifest.kind {
        DraftKind::Create if manifest.base_revision.is_some() => {
            bail!("create draft must not contain a base revision")
        }
        DraftKind::Update => {
            let revision = manifest
                .base_revision
                .as_deref()
                .context("update draft is missing its base revision")?;
            if revision.len() != 64 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("invalid update draft base revision");
            }
        }
        DraftKind::Create => {}
    }
    let now = unix_time(SystemTime::now());
    if manifest.created_at > now.saturating_add(300) {
        bail!("skill draft creation timestamp is in the future");
    }
    Ok(manifest)
}

fn validate_draft_id(id: &str) -> Result<()> {
    let mut components = Path::new(id).components();
    if !id.starts_with("draft-")
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        bail!("invalid skill draft id");
    }
    Ok(())
}

fn validate_skill_package(root: &Path, expected_name: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("skill package root must be a regular directory");
    }
    let raw = read_skill_file(&root.join("SKILL.md"))?;
    parse_skill_metadata(&raw, Some(expected_name))?;
    let mut stats = PackageStats {
        directories: 1,
        ..PackageStats::default()
    };
    validate_package_tree(root, 0, &mut stats)?;
    Ok(())
}

#[derive(Default)]
struct PackageStats {
    files: usize,
    directories: usize,
    bytes: u64,
}

fn validate_package_tree(path: &Path, depth: usize, stats: &mut PackageStats) -> Result<()> {
    if depth > MAX_SKILL_PACKAGE_DEPTH {
        bail!("skill package exceeds the directory depth limit");
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(
                "skill packages may not contain symbolic links: {}",
                entry.path().display()
            );
        }
        if metadata.is_dir() {
            stats.directories += 1;
            if stats.directories > MAX_SKILL_PACKAGE_DIRS {
                bail!("skill package exceeds the directory-count limit");
            }
            validate_package_tree(&entry.path(), depth + 1, stats)?;
        } else if metadata.is_file() {
            stats.files += 1;
            stats.bytes = stats
                .bytes
                .checked_add(metadata.len())
                .context("skill package size overflow")?;
            if stats.files > MAX_SKILL_PACKAGE_FILES || stats.bytes > MAX_SKILL_PACKAGE_BYTES {
                bail!("skill package exceeds file-count or total-size limits");
            }
        } else {
            bail!(
                "skill package contains an unsupported file type: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn skill_revision(root: &Path) -> Result<String> {
    let mut entries = Vec::new();
    let mut stats = PackageStats {
        directories: 1,
        ..PackageStats::default()
    };
    collect_revision_entries(root, root, 0, &mut stats, &mut entries)?;
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    for entry in entries {
        hasher.update(&[entry.kind]);
        hash_length_prefixed(&mut hasher, entry.relative.as_os_str().as_encoded_bytes());
        hasher.update(&entry.mode.to_le_bytes());
        hasher.update(&entry.length.to_le_bytes());
        if entry.kind == b'f' {
            let mut file = File::open(entry.path)?;
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

struct RevisionEntry {
    relative: PathBuf,
    path: PathBuf,
    kind: u8,
    mode: u32,
    length: u64,
}

fn collect_revision_entries(
    root: &Path,
    path: &Path,
    depth: usize,
    stats: &mut PackageStats,
    entries: &mut Vec<RevisionEntry>,
) -> Result<()> {
    if depth > MAX_SKILL_PACKAGE_DEPTH {
        bail!("skill package exceeds the directory depth limit");
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!("skill packages may not contain symbolic links");
        }
        let relative = entry.path().strip_prefix(root)?.to_path_buf();
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            metadata.mode()
        };
        #[cfg(not(unix))]
        let mode = 0;
        if metadata.is_dir() {
            stats.directories += 1;
            if stats.directories > MAX_SKILL_PACKAGE_DIRS {
                bail!("skill package exceeds the directory-count limit");
            }
            entries.push(RevisionEntry {
                relative,
                path: entry.path(),
                kind: b'd',
                mode,
                length: 0,
            });
            collect_revision_entries(root, &entry.path(), depth + 1, stats, entries)?;
        } else if metadata.is_file() {
            stats.files += 1;
            stats.bytes = stats
                .bytes
                .checked_add(metadata.len())
                .context("skill package size overflow")?;
            if stats.files > MAX_SKILL_PACKAGE_FILES || stats.bytes > MAX_SKILL_PACKAGE_BYTES {
                bail!("skill package exceeds file-count or total-size limits");
            }
            entries.push(RevisionEntry {
                relative,
                path: entry.path(),
                kind: b'f',
                mode,
                length: metadata.len(),
            });
        } else {
            bail!("skill package contains an unsupported file type");
        }
    }
    Ok(())
}

fn hash_length_prefixed(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let mut stats = PackageStats {
        directories: 1,
        ..PackageStats::default()
    };
    fs::create_dir(destination)?;
    let result = (|| {
        secure_directory(destination)?;
        copy_tree_inner(source, destination, 0, &mut stats)?;
        if let Some(parent) = destination.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

fn copy_tree_inner(
    source: &Path,
    destination: &Path,
    depth: usize,
    stats: &mut PackageStats,
) -> Result<()> {
    if depth > MAX_SKILL_PACKAGE_DEPTH {
        bail!("skill package exceeds the directory depth limit");
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let target = destination.join(entry.file_name());
        if metadata.file_type().is_symlink() {
            bail!("skill packages may not contain symbolic links");
        }
        if metadata.is_dir() {
            stats.directories += 1;
            if stats.directories > MAX_SKILL_PACKAGE_DIRS {
                bail!("skill package exceeds the directory-count limit");
            }
            fs::create_dir(&target)?;
            secure_directory(&target)?;
            copy_tree_inner(&entry.path(), &target, depth + 1, stats)?;
        } else if metadata.is_file() {
            stats.files += 1;
            stats.bytes = stats
                .bytes
                .checked_add(metadata.len())
                .context("skill package size overflow")?;
            if stats.files > MAX_SKILL_PACKAGE_FILES || stats.bytes > MAX_SKILL_PACKAGE_BYTES {
                bail!("skill package exceeds file-count or total-size limits");
            }
            fs::copy(entry.path(), &target)?;
            File::open(&target)?.sync_all()?;
        } else {
            bail!("skill package contains an unsupported file type");
        }
    }
    File::open(destination)?.sync_all()?;
    Ok(())
}

fn ensure_directory_chain(base: &Path, directory: &Path) -> Result<()> {
    let relative = directory
        .strip_prefix(base)
        .with_context(|| format!("path escapes skill root: {}", directory.display()))?;
    let mut current = base.to_path_buf();
    let metadata = fs::symlink_metadata(&current)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "skill path contains an unsafe directory: {}",
            current.display()
        );
    }
    for component in relative.components() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "skill path contains an unsafe directory: {}",
                current.display()
            );
        }
    }
    Ok(())
}

fn create_private_directory_chain(base: &Path, directory: &Path) -> Result<()> {
    let relative = directory
        .strip_prefix(base)
        .with_context(|| format!("path escapes skill root: {}", directory.display()))?;
    secure_directory(base)?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => secure_directory(&current)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                secure_directory(&current)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

struct StagedDirectory {
    path: PathBuf,
    armed: bool,
}

impl StagedDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagedDirectory {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn exchange_skill_directories(staged: &Path, target: &Path) -> Result<()> {
    exchange_directories(staged, target).with_context(|| {
        format!(
            "atomically exchanging skill directories {} and {}",
            staged.display(),
            target.display()
        )
    })?;
    if let Some(parent) = target.parent() {
        if let Err(error) = File::open(parent).and_then(|directory| directory.sync_all()) {
            tracing::warn!(path = %parent.display(), error = %error, "failed to sync updated skill directory");
        }
    }
    Ok(())
}

fn install_updated_skill(
    staged: &Path,
    target: &Path,
    expected_revision: &str,
    staged_guard: &mut StagedDirectory,
) -> Result<()> {
    exchange_skill_directories(staged, target)?;
    let replaced_revision = skill_revision(staged);
    let replaced_matches = matches!(
        replaced_revision.as_deref(),
        Ok(revision) if revision == expected_revision
    );
    if !replaced_matches {
        if let Err(rollback_error) = exchange_skill_directories(staged, target) {
            staged_guard.disarm();
            bail!(
                "live skill changed during publication and rollback failed; the old version is preserved at {}: {rollback_error:#}",
                staged.display()
            );
        }
        bail!("skill changed during publication; the live version was restored");
    }
    if let Err(error) = fs::remove_dir_all(staged) {
        tracing::warn!(path = %staged.display(), error = %error, "failed to remove replaced skill directory");
    }
    staged_guard.disarm();
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_new_skill(staged: &Path, target: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;

    let (parent, staged, target) = exchange_operands(staged, target)?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            staged.as_ptr(),
            parent.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_new_skill(staged: &Path, target: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;

    let (parent, staged, target) = exchange_operands(staged, target)?;
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            staged.as_ptr(),
            parent.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn install_new_skill(staged: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        bail!("skill already exists; create never overwrites");
    }
    fs::rename(staged, target)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn exchange_directories(left: &Path, right: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;

    let (parent, left, right) = exchange_operands(left, right)?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            left.as_ptr(),
            parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn exchange_directories(left: &Path, right: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;

    let (parent, left, right) = exchange_operands(left, right)?;
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            left.as_ptr(),
            parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exchange_directories(_left: &Path, _right: &Path) -> Result<()> {
    bail!("atomic skill updates are unsupported on this platform")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exchange_operands(
    left: &Path,
    right: &Path,
) -> Result<(File, std::ffi::CString, std::ffi::CString)> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = left.parent().context("staged skill has no parent")?;
    if right.parent() != Some(parent) {
        bail!("atomic skill exchange requires a shared parent directory");
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let parent = options.open(parent)?;
    let left = std::ffi::CString::new(
        left.file_name()
            .context("staged skill has no file name")?
            .as_bytes(),
    )?;
    let right = std::ffi::CString::new(
        right
            .file_name()
            .context("target skill has no file name")?
            .as_bytes(),
    )?;
    Ok((parent, left, right))
}

struct PublishLease {
    _file: File,
}

fn acquire_publish_lock(paths: &MiyuPaths) -> Result<PublishLease> {
    let root = paths.skill_drafts_dir();
    create_private_dir(&root)?;
    let lock_path = root.join(PUBLISH_LOCK_FILE);
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("skill publish lock path is unsafe: {}", lock_path.display())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    sys::set_open_options_mode(&mut options, 0o600);
    let file = options.open(&lock_path)?;
    sys::flock_lock_ex(&file, false)?;
    Ok(PublishLease { _file: file })
}

enum DraftInspection {
    Valid(SystemTime),
    Invalid,
}

fn latest_modified(path: &Path) -> Result<SystemTime> {
    match inspect_latest_modified(path)? {
        DraftInspection::Valid(modified) => Ok(modified),
        DraftInspection::Invalid => bail!("skill draft exceeds inspection limits"),
    }
}

fn inspect_latest_modified(path: &Path) -> Result<DraftInspection> {
    let mut latest = UNIX_EPOCH;
    let mut visited = 0usize;
    let mut pending = vec![(path.to_path_buf(), 0usize)];
    while let Some((path, depth)) = pending.pop() {
        visited += 1;
        if visited > MAX_SKILL_PACKAGE_FILES + MAX_SKILL_PACKAGE_DIRS + 16 {
            return Ok(DraftInspection::Invalid);
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if modified > latest {
            latest = modified;
        }
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            if depth > MAX_SKILL_PACKAGE_DEPTH + 4 {
                return Ok(DraftInspection::Invalid);
            }
            for entry in fs::read_dir(path)? {
                match entry {
                    Ok(entry) => pending.push((entry.path(), depth + 1)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok(DraftInspection::Valid(latest))
}

fn unix_time(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    secure_directory(path)
}

fn secure_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("expected a regular directory: {}", path.display());
    }
    sys::set_secure_dir_permissions(path)?;
    Ok(())
}

fn write_private_file(path: &Path, content: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    sys::set_open_options_mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(content)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(root: &Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("data/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("data/pictures"),
            fish_hook_file: root.join("fish/miyu.fish"),
            bash_hook_file: root.join("config/shell/bash-hook.sh"),
            zsh_hook_file: root.join("config/shell/zsh-hook.zsh"),
            scripts_dir: root.join("data/scripts"),
            system_scripts_dir: PathBuf::new(),
        }
    }

    #[test]
    fn parses_standard_frontmatter_fields() {
        let raw = "---\nname: sample-skill\ndescription: Sample workflow\nlicense: MIT\ncompatibility: Miyu\nallowed-tools: read_file\nmetadata:\n  author: test\n---\n\nBody.";
        let metadata = parse_skill_metadata(raw, Some("sample-skill")).unwrap();
        assert_eq!(metadata.license.as_deref(), Some("MIT"));
        assert_eq!(metadata.compatibility.as_deref(), Some("Miyu"));
        assert_eq!(metadata.allowed_tools.as_deref(), Some("read_file"));
        assert_eq!(
            metadata.metadata.get("author").map(String::as_str),
            Some("test")
        );
    }

    #[test]
    fn rejects_yaml_anchors_before_loading_frontmatter() {
        let raw = "---\nname: sample-skill\ndescription: &description Sample workflow\nmetadata:\n  copied: *description\n---\n";
        let error = parse_skill_metadata(raw, Some("sample-skill")).unwrap_err();
        assert!(error.to_string().contains("anchors or aliases"));
    }

    #[test]
    fn persona_skill_overrides_global_and_builtin() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let global = paths.skills_dir.join(BUILTIN_SKILLS[0].0);
        let persona = config
            .active_persona_skills_dir(&paths)
            .join(BUILTIN_SKILLS[0].0);
        for (directory, description) in [(&global, "global"), (&persona, "persona")] {
            fs::create_dir_all(directory).unwrap();
            fs::write(
                directory.join("SKILL.md"),
                format!(
                    "---\nname: {}\ndescription: {description}\n---\n",
                    BUILTIN_SKILLS[0].0
                ),
            )
            .unwrap();
        }
        let entries = discover(&config, &paths).unwrap();
        let creator = entries
            .iter()
            .find(|entry| entry.metadata.name == BUILTIN_SKILLS[0].0)
            .unwrap();
        assert_eq!(creator.source, SkillSource::Persona);
        assert_eq!(creator.metadata.description, "persona");
    }

    #[test]
    fn create_and_publish_draft_never_overwrites() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let draft = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let published = publish_draft(&paths, &draft.id).unwrap();
        assert!(Path::new(&published.path).join("SKILL.md").is_file());
        assert!(create_draft(
            &config,
            &paths,
            "sample-skill",
            "Duplicate",
            SkillScope::Global,
        )
        .is_err());
    }

    #[test]
    fn deletes_global_and_current_persona_skills() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        for scope in [SkillScope::Global, SkillScope::Persona] {
            let draft = create_draft(
                &config,
                &paths,
                "sample-skill",
                "Use for sample tasks",
                scope,
            )
            .unwrap();
            publish_draft(&paths, &draft.id).unwrap();
        }

        let global = delete_skill(&config, &paths, "sample-skill", SkillScope::Global).unwrap();
        assert_eq!(global.scope, "global");
        assert!(!paths.skills_dir.join("sample-skill").exists());
        assert!(config
            .active_persona_skills_dir(&paths)
            .join("sample-skill")
            .is_dir());

        let persona = delete_skill(&config, &paths, "sample-skill", SkillScope::Persona).unwrap();
        assert_eq!(persona.scope, "persona");
        assert!(!config
            .active_persona_skills_dir(&paths)
            .join("sample-skill")
            .exists());
        assert!(delete_skill(&config, &paths, "sample-skill", SkillScope::Global).is_err());
    }

    #[test]
    fn update_draft_detects_concurrent_edits() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let created = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        publish_draft(&paths, &created.id).unwrap();
        let update = update_draft(&config, &paths, "sample-skill", SkillScope::Global).unwrap();
        fs::write(
            paths.skills_dir.join("sample-skill/SKILL.md"),
            "---\nname: sample-skill\ndescription: Changed elsewhere\n---\n",
        )
        .unwrap();
        assert!(publish_draft(&paths, &update.id).is_err());
    }

    #[test]
    fn two_update_drafts_from_the_same_revision_cannot_both_publish() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let created = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        publish_draft(&paths, &created.id).unwrap();
        let first = update_draft(&config, &paths, "sample-skill", SkillScope::Global).unwrap();
        let second = update_draft(&config, &paths, "sample-skill", SkillScope::Global).unwrap();
        fs::write(
            &first.skill_file,
            "---\nname: sample-skill\ndescription: First update\n---\n",
        )
        .unwrap();
        fs::write(
            &second.skill_file,
            "---\nname: sample-skill\ndescription: Second update\n---\n",
        )
        .unwrap();

        publish_draft(&paths, &first.id).unwrap();
        assert!(publish_draft(&paths, &second.id).is_err());
        assert!(
            fs::read_to_string(paths.skills_dir.join("sample-skill/SKILL.md"))
                .unwrap()
                .contains("First update")
        );
    }

    #[test]
    fn live_edit_detected_after_exchange_is_atomically_restored() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("sample-skill");
        let staged = temp.path().join(".stage");
        for (directory, description) in [(&target, "Original"), (&staged, "Updated")] {
            fs::create_dir(directory).unwrap();
            fs::write(
                directory.join("SKILL.md"),
                format!("---\nname: sample-skill\ndescription: {description}\n---\n"),
            )
            .unwrap();
        }
        let expected = skill_revision(&target).unwrap();
        fs::write(
            target.join("SKILL.md"),
            "---\nname: sample-skill\ndescription: Manual edit\n---\n",
        )
        .unwrap();

        let mut guard = StagedDirectory::new(staged.clone());
        assert!(install_updated_skill(&staged, &target, &expected, &mut guard).is_err());
        assert!(fs::read_to_string(target.join("SKILL.md"))
            .unwrap()
            .contains("Manual edit"));
        assert!(fs::read_to_string(staged.join("SKILL.md"))
            .unwrap()
            .contains("Updated"));
    }

    #[test]
    fn tampered_persona_scope_cannot_escape_the_skill_root() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let draft = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Persona,
        )
        .unwrap();
        let manifest_path = paths
            .skill_drafts_dir()
            .join(&draft.id)
            .join(DRAFT_MANIFEST);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["persona_scope"] = serde_json::Value::String("../../outside".to_string());
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        assert!(publish_draft(&paths, &draft.id).is_err());
        assert!(!paths.data_dir.join("outside/sample-skill").exists());
    }

    #[cfg(unix)]
    #[test]
    fn publish_rejects_a_symlinked_draft_package() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let draft = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let draft_root = paths.skill_drafts_dir().join(&draft.id);
        let package = draft_root.join(DRAFT_PACKAGE_DIR);
        let outside = temp.path().join("outside-package");
        fs::create_dir_all(outside.join("sample-skill")).unwrap();
        fs::write(
            outside.join("sample-skill/SKILL.md"),
            "---\nname: sample-skill\ndescription: Outside\n---\n",
        )
        .unwrap();
        fs::remove_dir_all(&package).unwrap();
        symlink(&outside, &package).unwrap();

        assert!(publish_draft(&paths, &draft.id).is_err());
        assert!(!paths.skills_dir.join("sample-skill").exists());
    }

    #[test]
    fn expired_draft_cannot_be_published_directly() {
        fn set_modified_recursive(path: &Path, modified: SystemTime) {
            if path.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    set_modified_recursive(&entry.unwrap().path(), modified);
                }
            }
            File::open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let draft = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let draft_root = paths.skill_drafts_dir().join(&draft.id);
        let expired = SystemTime::now() - DRAFT_RETENTION - Duration::from_secs(60);
        set_modified_recursive(&draft_root, expired);

        assert!(publish_draft(&paths, &draft.id).is_err());
        assert!(!draft_root.exists());
        assert!(!paths.skills_dir.join("sample-skill").exists());
    }

    #[test]
    fn malformed_over_limit_draft_is_removed_during_pruning() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let draft = create_draft(
            &AppConfig::default(),
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let mut directory = PathBuf::from(&draft.skill_dir);
        for index in 0..=(MAX_SKILL_PACKAGE_DEPTH + 5) {
            directory.push(format!("level-{index}"));
        }
        fs::create_dir_all(directory).unwrap();

        assert_eq!(prune_expired_drafts(&paths).unwrap(), 1);
        assert!(!paths.skill_drafts_dir().join(&draft.id).exists());
    }

    #[test]
    fn future_draft_timestamps_are_not_treated_as_expired() {
        fn set_modified_recursive(path: &Path, modified: SystemTime) {
            if path.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    set_modified_recursive(&entry.unwrap().path(), modified);
                }
            }
            File::open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let draft = create_draft(
            &AppConfig::default(),
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let draft_root = paths.skill_drafts_dir().join(&draft.id);
        set_modified_recursive(
            &draft_root,
            SystemTime::now() + Duration::from_secs(24 * 60 * 60),
        );

        assert_eq!(prune_expired_drafts(&paths).unwrap(), 0);
        assert!(draft_root.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn revision_tracks_empty_directories_and_executable_bits() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sample-skill");
        fs::create_dir_all(&root).unwrap();
        let skill_file = root.join("SKILL.md");
        fs::write(
            &skill_file,
            "---\nname: sample-skill\ndescription: Sample\n---\n",
        )
        .unwrap();
        let initial = skill_revision(&root).unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        let with_directory = skill_revision(&root).unwrap();
        assert_ne!(initial, with_directory);
        let mut permissions = fs::metadata(&skill_file).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&skill_file, permissions).unwrap();
        assert_ne!(with_directory, skill_revision(&root).unwrap());
    }

    #[test]
    fn publish_rejects_excessive_directory_depth() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let draft = create_draft(
            &config,
            &paths,
            "sample-skill",
            "Use for sample tasks",
            SkillScope::Global,
        )
        .unwrap();
        let mut directory = PathBuf::from(&draft.skill_dir);
        for index in 0..=MAX_SKILL_PACKAGE_DEPTH {
            directory.push(format!("level-{index}"));
        }
        fs::create_dir_all(directory).unwrap();

        assert!(publish_draft(&paths, &draft.id).is_err());
        assert!(!paths.skills_dir.join("sample-skill").exists());
    }
}
