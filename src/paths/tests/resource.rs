//! 资源布局迁移（预检、日志、回滚、并发排他）。

use super::shared::*;
use crate::paths::*;

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
    // 这条用的是真 ~/.miyu 的根:机器已经搬成家目录布局时脚本住 extensions/。
    let scripts_root = if paths.home_admin().is_some() {
        root.join("extensions/scripts/images")
    } else {
        root.join("data/scripts/images")
    };
    assert_eq!(
        paths.migrated_resource_path(Path::new("prompts/../scripts/images")),
        Some(scripts_root)
    );
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
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

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
    assert_eq!(
        unsafe { libc::flock(starter.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

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
