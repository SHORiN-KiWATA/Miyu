//! 家目录布局迁移(阶段 6):搬家、回滚、计划、新装即新布局、路径解析。

use super::shared::*;
use crate::paths::*;

fn home_layout(root: &Path, admin: &str) -> HomeLayout {
    let (_, layout) = test_layouts(root);
    HomeLayout {
        layout,
        admin: admin.to_string(),
    }
}

fn seed_legacy_tree(layout: &HomeLayout) {
    let data = &layout.layout.data_dir;
    let state = &layout.layout.state_dir;
    fs::create_dir_all(data.join("personas/default/memory")).unwrap();
    fs::write(data.join("personas/default/memory/memory.db"), "mem").unwrap();
    fs::create_dir_all(data.join("skills/demo")).unwrap();
    fs::write(data.join("skills/demo/SKILL.md"), "skill").unwrap();
    fs::create_dir_all(data.join("scripts")).unwrap();
    fs::write(data.join("scripts/tool.sh"), "#!/bin/sh\n").unwrap();
    fs::create_dir_all(data.join("identities/team")).unwrap();
    fs::write(data.join("identities/user-identity.md"), "I am shorin").unwrap();
    fs::write(data.join("identities/team/user.md"), "team view").unwrap();
    for name in ["artifacts", "documents", "pictures", "ledger", "shared"] {
        fs::create_dir_all(data.join(name)).unwrap();
        fs::write(data.join(name).join("file"), name).unwrap();
    }
    fs::create_dir_all(state).unwrap();
    fs::write(state.join("conversation.db"), "db").unwrap();
    fs::write(state.join("conversation.db-wal"), "wal").unwrap();
    fs::write(state.join("usage.json"), "{}").unwrap();
    // 不在清单里的东西原地不动
    fs::create_dir_all(data.join("kb")).unwrap();
    fs::write(data.join("kb/meta"), "kb").unwrap();
}

#[test]
fn home_layout_migration_moves_owner_data_and_records_the_admin() {
    let temp = tempfile::tempdir().unwrap();
    let layout = home_layout(temp.path(), "shorin");
    seed_legacy_tree(&layout);

    assert!(try_migrate_home_layout(&layout, false).unwrap());

    let root = &layout.layout.root_dir;
    let home = root.join("home/shorin");
    assert_eq!(
        read_home_layout_admin(root).unwrap().as_deref(),
        Some("shorin")
    );
    assert!(root.join("personas/default/memory/memory.db").is_file());
    assert!(root.join("extensions/skills/demo/SKILL.md").is_file());
    assert!(root.join("extensions/scripts/tool.sh").is_file());
    assert!(home.join("conversation.db").is_file());
    assert!(home.join("conversation.db-wal").is_file());
    assert!(home.join("profile.md").is_file());
    assert_eq!(
        fs::read_to_string(home.join("profile.md")).unwrap(),
        "I am shorin"
    );
    assert!(home.join("identities/team/user.md").is_file());
    assert!(!home.join("identities/user-identity.md").exists());
    for name in ["artifacts", "documents", "pictures", "ledger"] {
        assert!(home.join(name).join("file").is_file(), "{name}");
    }
    assert!(home.join("shares/file").is_file());
    // 原地不动的
    assert!(layout.layout.data_dir.join("kb/meta").is_file());
    assert!(layout.layout.state_dir.join("usage.json").is_file());
    assert!(!layout.layout.data_dir.join("personas").exists());
    assert!(!layout.layout.state_dir.join("conversation.db").exists());
    assert!(!layout.journal().exists());
    // 再跑一次是幂等的
    assert!(try_migrate_home_layout(&layout, false).unwrap());
}

#[test]
fn home_layout_rollback_restores_the_legacy_tree() {
    let temp = tempfile::tempdir().unwrap();
    let layout = home_layout(temp.path(), "shorin");
    seed_legacy_tree(&layout);
    assert!(try_migrate_home_layout(&layout, false).unwrap());

    assert!(rollback_home_layout(&layout, false).unwrap());

    let data = &layout.layout.data_dir;
    let state = &layout.layout.state_dir;
    assert!(read_home_layout_admin(&layout.layout.root_dir)
        .unwrap()
        .is_none());
    assert!(data.join("personas/default/memory/memory.db").is_file());
    assert!(data.join("skills/demo/SKILL.md").is_file());
    assert!(data.join("identities/user-identity.md").is_file());
    assert!(data.join("identities/team/user.md").is_file());
    assert!(data.join("shared/file").is_file());
    assert!(state.join("conversation.db").is_file());
    assert!(!layout
        .layout
        .root_dir
        .join("home/shorin/profile.md")
        .exists());
    assert!(!layout.layout.root_dir.join("personas").exists());
    assert!(!layout.journal().exists());
    // 回滚留下「别再自动搬」标记;--apply 撤掉它
    assert!(home_layout_opted_out(&layout.layout.root_dir).unwrap());
    clear_home_layout_opt_out(&layout).unwrap();
    assert!(!home_layout_opted_out(&layout.layout.root_dir).unwrap());
}

#[test]
fn fresh_install_gets_the_home_layout_without_moving_anything() {
    let temp = tempfile::tempdir().unwrap();
    let layout = home_layout(temp.path(), "alice");
    fs::create_dir_all(&layout.layout.root_dir).unwrap();
    let plan = layout.plan().unwrap();
    assert!(plan.iter().all(|step| !step.present));
    assert!(try_migrate_home_layout(&layout, false).unwrap());
    assert_eq!(
        read_home_layout_admin(&layout.layout.root_dir)
            .unwrap()
            .as_deref(),
        Some("alice")
    );
    assert!(layout.admin_home().is_dir());
}

#[test]
fn home_layout_conflict_has_no_writes() {
    let temp = tempfile::tempdir().unwrap();
    let layout = home_layout(temp.path(), "shorin");
    seed_legacy_tree(&layout);
    // 目标已存在:整个搬家拒绝,一样都不动
    fs::create_dir_all(layout.layout.root_dir.join("personas")).unwrap();
    let error = try_migrate_home_layout(&layout, false).unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error:#}");
    assert!(layout
        .layout
        .data_dir
        .join("personas/default/memory/memory.db")
        .is_file());
    assert!(layout.layout.state_dir.join("conversation.db").is_file());
    assert!(read_home_layout_admin(&layout.layout.root_dir)
        .unwrap()
        .is_none());
}

#[test]
fn admin_home_names_follow_username_rules() {
    assert!(is_valid_home_name("shorin"));
    assert!(is_valid_home_name("a.b-c_9"));
    assert!(!is_valid_home_name("ab"));
    assert!(!is_valid_home_name("..\u{0}"));
    assert!(!is_valid_home_name("-lead"));
    assert!(!is_valid_home_name("with space"));
    assert!(!is_valid_home_name("../etc"));
}

#[test]
fn miyu_paths_resolve_owner_data_through_the_marker() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".miyu");
    let paths = MiyuPaths {
        root_dir: root.clone(),
        config_dir: root.join("config"),
        config_file: root.join("config/config.jsonc"),
        skills_dir: root.join("extensions/skills"),
        data_dir: root.join("data"),
        cache_dir: root.join("cache"),
        state_dir: root.join("state"),
        pictures_dir: root.join("home/shorin/pictures"),
        fish_hook_file: root.join("fish/miyu.fish"),
        bash_hook_file: root.join("config/shell/bash-hook.sh"),
        zsh_hook_file: root.join("config/shell/zsh-hook.zsh"),
        scripts_dir: root.join("extensions/scripts"),
        system_scripts_dir: PathBuf::new(),
    };
    // 没标记:老布局
    assert!(paths.home_admin().is_none());
    assert_eq!(paths.personas_dir(), root.join("data/personas"));
    assert_eq!(paths.conversation_db_dir(), root.join("state"));
    assert_eq!(paths.artifacts_dir(), root.join("data/artifacts"));
    assert_eq!(
        paths.profile_file(),
        root.join("data/identities/user-identity.md")
    );

    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(HOME_LAYOUT_MARKER), "shorin\n").unwrap();
    assert_eq!(paths.home_admin().as_deref(), Some("shorin"));
    let home = root.join("home/shorin");
    assert_eq!(paths.admin_home_dir().as_deref(), Some(home.as_path()));
    assert_eq!(paths.personas_dir(), root.join("personas"));
    assert_eq!(paths.conversation_db_dir(), home);
    assert_eq!(paths.artifacts_dir(), home.join("artifacts"));
    assert_eq!(paths.documents_dir(), home.join("documents"));
    assert_eq!(paths.ledger_dir(), home.join("ledger"));
    assert_eq!(paths.shared_files_dir(), home.join("shares"));
    assert_eq!(paths.profile_file(), home.join("profile.md"));
    assert_eq!(paths.identities_dir(), home.join("identities"));
    assert_eq!(
        paths.user_profile_file("alice"),
        root.join("home/alice/profile.md")
    );
    // 资源根仍是 data(prompts / persona-avatars 没搬),skills/scripts 解析到 extensions
    assert_eq!(paths.resource_dir(), root.join("data"));
    assert_eq!(paths.prompts_dir(), root.join("data/prompts"));
    assert_eq!(
        paths.migrated_resource_path(Path::new("skills/demo")),
        Some(root.join("extensions/skills/demo"))
    );
    assert_eq!(
        paths.migrated_resource_path(Path::new("identities/user-identity.md")),
        Some(home.join("profile.md"))
    );
    assert_eq!(
        paths.migrated_resource_path(Path::new("identities/team/user.md")),
        Some(home.join("identities/team/user.md"))
    );
    assert_eq!(
        paths.migrated_resource_path(Path::new("prompts/persona.md")),
        Some(root.join("data/prompts/persona.md"))
    );

    // 坏标记不当新布局
    fs::write(root.join(HOME_LAYOUT_MARKER), "../etc\n").unwrap();
    assert!(read_home_layout_admin(&root).is_err());
    assert!(paths.home_admin().is_none());
}
