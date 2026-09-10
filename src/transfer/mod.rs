//! Moving a Miyu installation between machines: `miyu export` / `miyu import`.

pub mod export;
pub mod fixups;
pub mod import;
pub mod manifest;
pub mod registry;

#[cfg(test)]
pub(crate) mod tests {
    use super::registry::{is_backup_name, unit_for, Tier, IGNORED_SUFFIXES, UNITS};
    use crate::paths::MiyuPaths;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// A MiyuPaths rooted at `root`, mirroring the real layout.
    pub(crate) fn test_paths(root: &Path) -> MiyuPaths {
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
            system_scripts_dir: root.join("system-scripts"),
        }
    }

    /// Everything Miyu writes under `MIYU_HOME` must be classified.
    ///
    /// This is the guard that keeps export from rotting: add a feature that
    /// writes somewhere new, forget to register it, and this fails with a
    /// pointer to the registry — instead of the user discovering the gap on
    /// the new machine, after the old one is gone.
    /// Builds a populated home: config with a secret, a database holding a
    /// row, a user resource, and things that must not travel.
    fn populated_home(root: &Path) -> MiyuPaths {
        let paths = test_paths(root);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::create_dir_all(paths.data_dir.join("prompts")).unwrap();
        std::fs::create_dir_all(root.join("cache/logs")).unwrap();
        std::fs::write(
            &paths.config_file,
            r#"{ "providers": [ { "id": "p", "api_key": "sk-secret" } ] }"#,
        )
        .unwrap();
        std::fs::write(paths.data_dir.join("prompts/system-prompt.md"), "persona").unwrap();
        std::fs::write(root.join("cache/logs/miyu.log"), "noise").unwrap();
        std::fs::write(paths.state_dir.join("conversation.db.bak"), "old").unwrap();

        let conn = rusqlite::Connection::open(paths.state_dir.join("conversation.db")).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA user_version=17;
             CREATE TABLE sessions (session_id TEXT PRIMARY KEY, workspace TEXT);
             CREATE TABLE turns (turn_id TEXT PRIMARY KEY, workspace TEXT,
                                 tool_footprint TEXT, owner_pid INTEGER);
             CREATE TABLE queued_prompts (prompt_id TEXT PRIMARY KEY, owner_pid INTEGER);
             INSERT INTO sessions VALUES ('s1', '/gone/from/this/machine');",
        )
        .unwrap();
        // Leave the write sitting in the WAL: a plain file copy would miss it.
        std::mem::forget(conn);
        paths
    }

    #[test]
    fn an_export_round_trips_into_an_empty_home() {
        let source = tempfile::tempdir().unwrap();
        let paths = populated_home(source.path());
        let out = tempfile::tempdir().unwrap();
        let archive = out.path().join("miyu-export.tar.gz");

        let report =
            super::export::export(&paths, &archive, &super::export::ExportOptions::default())
                .unwrap();
        assert!(report.entries > 0);
        assert!(report.secrets_included);

        let target = tempfile::tempdir().unwrap();
        let restored = test_paths(target.path());
        std::fs::create_dir_all(&restored.config_dir).unwrap();
        std::fs::remove_dir_all(&restored.config_dir).unwrap();
        let outcome = super::import::import(
            &restored,
            &archive,
            &super::import::ImportOptions::default(),
        )
        .unwrap();
        assert!(outcome.restored > 0);
        assert!(outcome.unknown_units.is_empty());

        // Config and user resources came across, secrets intact.
        let config = std::fs::read_to_string(&restored.config_file).unwrap();
        assert!(config.contains("sk-secret"));
        assert_eq!(
            std::fs::read_to_string(restored.data_dir.join("prompts/system-prompt.md")).unwrap(),
            "persona"
        );

        // The database came through SQLite, so the row that was still in the
        // WAL is present — and its dead workspace was cleared on the way in.
        let conn = rusqlite::Connection::open(restored.state_dir.join("conversation.db")).unwrap();
        let workspace: Option<String> = conn
            .query_row(
                "SELECT workspace FROM sessions WHERE session_id='s1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(workspace.is_none(), "stale workspace should be cleared");
        assert_eq!(outcome.cleared_workspaces, 1);

        // Machine-specific noise stayed behind.
        assert!(!target.path().join("cache/logs/miyu.log").exists());
        assert!(!restored.state_dir.join("conversation.db.bak").exists());
        // The layout markers are stamped so the tree is not re-migrated.
        assert!(target.path().join(".layout-v1").exists());
    }

    #[test]
    fn importing_over_existing_data_is_refused_without_force() {
        let source = tempfile::tempdir().unwrap();
        let paths = populated_home(source.path());
        let out = tempfile::tempdir().unwrap();
        let archive = out.path().join("miyu-export.tar.gz");
        super::export::export(&paths, &archive, &super::export::ExportOptions::default()).unwrap();

        // The source home is itself non-empty, so importing onto it must stop.
        let error =
            super::import::import(&paths, &archive, &super::import::ImportOptions::default())
                .unwrap_err()
                .to_string();
        assert!(error.contains("--force"), "got: {error}");

        let outcome = super::import::import(
            &paths,
            &archive,
            &super::import::ImportOptions { force: true },
        )
        .unwrap();
        // Overwriting is only allowed after the current state is safe.
        let backup = outcome.backup.expect("--force must back up first");
        assert!(backup.exists());
    }

    #[test]
    fn no_secrets_blanks_credentials_without_dropping_the_config() {
        let source = tempfile::tempdir().unwrap();
        let paths = populated_home(source.path());
        let out = tempfile::tempdir().unwrap();
        let archive = out.path().join("redacted.tar.gz");
        let report = super::export::export(
            &paths,
            &archive,
            &super::export::ExportOptions {
                no_secrets: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!report.secrets_included);

        let target = tempfile::tempdir().unwrap();
        let restored = test_paths(target.path());
        super::import::import(
            &restored,
            &archive,
            &super::import::ImportOptions::default(),
        )
        .unwrap();
        let config = std::fs::read_to_string(&restored.config_file).unwrap();
        assert!(!config.contains("sk-secret"), "the key must not travel");
        assert!(
            config.contains("providers"),
            "the config itself must survive"
        );
    }

    #[test]
    fn a_newer_archive_is_refused_rather_than_downgraded() {
        let source = tempfile::tempdir().unwrap();
        let paths = populated_home(source.path());
        let out = tempfile::tempdir().unwrap();
        let archive = out.path().join("future.tar.gz");
        super::export::export(&paths, &archive, &super::export::ExportOptions::default()).unwrap();

        // Rewrite the manifest to claim a schema this build cannot open.
        let raw = std::fs::read(&archive).unwrap();
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(&raw[..]));
        let mut manifest: super::manifest::Manifest = tar
            .entries()
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .path()
                    .map(|path| path.to_string_lossy() == "manifest.json")
                    .unwrap_or(false)
            })
            .map(|entry| serde_json::from_reader(entry).unwrap())
            .unwrap();
        manifest
            .schema_versions
            .insert("state.conversation".to_string(), 9_999);
        let doctored = out.path().join("doctored.tar.gz");
        rewrite_manifest(&archive, &doctored, &manifest);

        let target = tempfile::tempdir().unwrap();
        let restored = test_paths(target.path());
        let error = super::import::import(
            &restored,
            &doctored,
            &super::import::ImportOptions::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("9999"), "got: {error}");
    }

    /// Copies an archive, replacing only its manifest.
    fn rewrite_manifest(from: &Path, to: &Path, manifest: &super::manifest::Manifest) {
        let file = std::fs::File::create(to).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let bytes = serde_json::to_vec_pretty(manifest).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        builder
            .append_data(&mut header, "manifest.json", bytes.as_slice())
            .unwrap();

        let raw = std::fs::read(from).unwrap();
        let mut source = tar::Archive::new(flate2::read::GzDecoder::new(&raw[..]));
        for entry in source.entries().unwrap().filter_map(Result::ok) {
            let path = entry.path().unwrap().to_path_buf();
            if path.to_string_lossy() == "manifest.json" {
                continue;
            }
            let mut header = entry.header().clone();
            header.set_cksum();
            builder.append(&header, entry).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn every_data_location_is_classified() {
        // A representative populated home. Kept literal rather than derived
        // from a live run so the expectations are reviewable in the diff.
        let observed = [
            ".layout-v1",
            ".resource-layout-v1",
            "cache/logs/miyu.2026-08-08.log",
            "cache/models_cache.json",
            "cache/jobs/abc123.log",
            "cache/clipboard_images/1.png",
            "cache/platform_images/onebot/x.jpg",
            "cache/default-kb/update-source",
            "config/config.jsonc",
            "config/webui-theme.css",
            "config/shell/bash-hook.sh",
            "data/prompts/system-prompt.md",
            "data/identities/user-identity.md",
            "data/persona-avatars/default.png",
            "data/skills/my-skill/SKILL.md",
            "data/scripts/index.json",
            "data/pictures/out.png",
            "data/documents/report.md",
            "data/memes/library/a.gif",
            "data/personas/default/memory/memory.db",
            "data/kb/files/games/a.md",
            "data/kb/kb_meta.db",
            "data/kb/semantic_index.db",
            "data/default-kb/state.json",
            "data/platforms/onebot/message_history/history.sqlite3",
            "data/platforms/onebot/real_context/state.json",
            "data/artifacts/sess_1/page.html",
            "state/conversation.db",
            "state/conversation.jsonl",
            "state/usage.json",
            "state/profile.md",
            "state/alarms.json",
            "state/thinking-variants.json",
            "state/repl-history.jsonl",
            "state/skill-drafts/draft.md",
            "state/personas/default/memory/evicted_context.db",
            "state/prompt-fingerprints/abc.sha256",
            "state/prompt.sha256",
            "state/aur-review-state.json",
            "state/arch_news_last_seen.json",
            "state/daemon-launch.json",
            "state/web-passwords/password-1234-ab",
            "state/miyu/core.sock",
            "state/conversation.db.bak",
            "data/shared/share_1/page.html",
            "data/personas/default/meme.db",
            "data/personas/default/persona.toml",
            ".home-layout-v1",
            ".home-layout-v1.journal.json",
            "personas/default/memory/memory.db",
            "personas/default/meme.db",
            "personas/default/persona.toml",
            "extensions/skills/my-skill/SKILL.md",
            "extensions/scripts/index.json",
            "home/shorin/profile.md",
            "home/shorin/identities/team/user.md",
            "home/shorin/conversation.db",
            "home/shorin/pictures/out.png",
            "home/shorin/documents/report.md",
            "home/shorin/ledger/ledger.db",
            "home/shorin/shares/share_1/page.html",
            "home/shorin/artifacts/sess_1/page.html",
            "home/alice/profile.md",
        ];

        let unclassified: Vec<&str> = observed
            .iter()
            .copied()
            .filter(|rel| unit_for(rel).is_none())
            .collect();
        assert!(
            unclassified.is_empty(),
            "unclassified paths under MIYU_HOME: {unclassified:?}\n\
             Register each in src/transfer/registry.rs `UNITS` — or mark it \
             Tier::Never with the reason it must not travel."
        );
    }

    #[test]
    fn an_unregistered_location_is_reported() {
        // The guard above is only worth having if it actually catches things.
        assert!(unit_for("data/brand-new-feature/store.db").is_none());
        assert!(unit_for("state/some-future-file.json").is_none());
    }

    #[test]
    fn unit_ids_and_paths_are_unique() {
        let ids: BTreeSet<&str> = UNITS.iter().map(|unit| unit.id).collect();
        assert_eq!(ids.len(), UNITS.len(), "duplicate DataUnit id");
        let rels: BTreeSet<&str> = UNITS.iter().map(|unit| unit.rel).collect();
        assert_eq!(rels.len(), UNITS.len(), "duplicate DataUnit rel");
    }

    #[test]
    fn never_units_state_a_reason() {
        for unit in UNITS.iter().filter(|unit| unit.tier == Tier::Never) {
            assert!(
                unit.why.len() > 20,
                "{}: Tier::Never needs a real reason, not `{}`",
                unit.id,
                unit.why
            );
        }
    }

    #[test]
    fn tier_switches_select_the_expected_units() {
        let ids = |all: bool, index: bool, platforms: bool| -> BTreeSet<&str> {
            UNITS
                .iter()
                .filter(|unit| unit.included(all, index, platforms))
                .map(|unit| unit.id)
                .collect()
        };

        let default = ids(false, false, false);
        assert!(default.contains("state.conversation"));
        assert!(default.contains("kb.files"));
        // The 143MB derived index and the account-bound platform history stay
        // out unless asked for.
        assert!(!default.contains("kb.semantic_index"));
        assert!(!default.contains("platform.message_history"));

        assert!(ids(false, true, false).contains("kb.semantic_index"));
        assert!(ids(false, false, true).contains("platform.message_history"));

        let all = ids(true, true, true);
        assert!(all.contains("kb.semantic_index"));
        assert!(all.contains("platform.message_history"));
        // No switch may ever pull in a Never unit.
        for unit in UNITS.iter().filter(|unit| unit.tier == Tier::Never) {
            assert!(!all.contains(unit.id), "{} must never be exported", unit.id);
        }
    }

    #[test]
    fn machine_specific_paths_resolve_to_never() {
        for rel in [
            "cache/logs/miyu.log",
            "cache/jobs/abc.log",
            "state/daemon-launch.json",
            "state/web-passwords/password-1-a",
            "state/miyu/core.sock",
            "config/shell/bash-hook.sh",
            "data/artifacts/sess_1/page.html",
            "state/conversation.db.bak",
        ] {
            let unit = unit_for(rel).unwrap_or_else(|| panic!("{rel} unclassified"));
            assert_eq!(unit.tier, Tier::Never, "{rel} resolved to {}", unit.id);
        }
    }

    #[test]
    fn sqlite_sidecars_and_backups_are_skipped() {
        for name in ["conversation.db-wal", "conversation.db-shm", "core.lock"] {
            assert!(
                IGNORED_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)),
                "{name} should be skipped as a sidecar"
            );
        }
        assert!(is_backup_name("config.jsonc.bak-20260802-011956"));
        assert!(is_backup_name("conversation.db.bak"));
        assert!(!is_backup_name("config.jsonc"));
        // Sanity: the helper is about names, not whole paths.
        assert!(Path::new("config.jsonc").file_name().is_some());
    }
}
