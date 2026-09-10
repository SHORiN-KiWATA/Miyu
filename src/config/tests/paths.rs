//! 路径与资源布局的迁移。

use crate::config::*;

#[test]
fn a_stale_xdg_output_dir_is_healed_and_its_files_follow() {
    // The value being healed is one an earlier upgrade wrote itself: it
    // remapped onto data_dir while data_dir still pointed at the legacy
    // XDG root, so the old root has to be a legacy root too.
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let legacy = home.join(".local/share/miyu/pictures/generated-images");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("one.png"), "a").unwrap();
    std::fs::write(legacy.join("two.png"), "b").unwrap();

    let destination_root = home.join(".miyu/data/pictures");
    let mut value = legacy.display().to_string();
    let moved = remap_managed_output_dir(
        &mut value,
        &[home.join(".local/share/miyu/pictures")],
        &destination_root,
        home,
    );
    let (from, to) = moved.expect("the stale root must be recognised");
    assert_eq!(to, destination_root.join("generated-images"));
    assert_eq!(value, to.display().to_string());

    relocate_managed_output(&from, &to);
    assert!(to.join("one.png").exists());
    assert!(to.join("two.png").exists());
    assert!(
        !from.exists(),
        "an emptied stale directory should not linger"
    );
}

#[test]
fn a_path_outside_every_legacy_root_is_left_alone() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let mut value = home.join("my-own-folder").display().to_string();
    let before = value.clone();
    let moved = remap_managed_output_dir(
        &mut value,
        &[home.join(".local/share/miyu/pictures")],
        &home.join(".miyu/data/pictures"),
        home,
    );
    assert!(moved.is_none());
    assert_eq!(value, before);
}

#[test]
fn default_prompt_resources_follow_the_data_resource_layout() {
    let temp = tempfile::tempdir().unwrap();
    let paths = MiyuPaths {
        root_dir: temp.path().to_path_buf(),
        config_dir: temp.path().join("config"),
        config_file: temp.path().join("config/config.jsonc"),
        skills_dir: temp.path().join("data/skills"),
        data_dir: temp.path().join("data"),
        cache_dir: temp.path().join("cache"),
        state_dir: temp.path().join("state"),
        pictures_dir: temp.path().join("data/pictures"),
        fish_hook_file: temp.path().join("fish/miyu.fish"),
        bash_hook_file: temp.path().join("config/shell/bash-hook.sh"),
        zsh_hook_file: temp.path().join("config/shell/zsh-hook.zsh"),
        scripts_dir: temp.path().join("data/scripts"),
        system_scripts_dir: PathBuf::new(),
    };
    let mut config = AppConfig::default();
    assert_eq!(
        config.prompts_dir_path(&paths),
        paths.data_dir.join("prompts")
    );
    assert_eq!(
        config.identities_dir_path(&paths),
        paths.data_dir.join("identities")
    );
    assert_eq!(
        config.user_identity_path(&paths),
        paths.data_dir.join("identities/user-identity.md")
    );
    assert_eq!(
        config.system_prompt_path(&paths),
        paths.data_dir.join("prompts/system-prompt.md")
    );

    config.prompt.prompts_dir = "./prompts/team".to_string();
    config.prompt.identities_dir = "identities/team".to_string();
    config.prompt.user_identity_file = "identities/team/user.md".to_string();
    config.system_prompt_file = Some("prompts/team/system.md".to_string());
    assert_eq!(
        config.prompts_dir_path(&paths),
        paths.data_dir.join("prompts/team")
    );
    assert_eq!(
        config.identities_dir_path(&paths),
        paths.data_dir.join("identities/team")
    );
    assert_eq!(
        config.user_identity_path(&paths),
        paths.data_dir.join("identities/team/user.md")
    );
    assert_eq!(
        config.system_prompt_path(&paths),
        paths.data_dir.join("prompts/team/system.md")
    );

    config.prompt.prompts_dir = "prompts/../scripts/personas".to_string();
    config.prompt.identities_dir = paths
        .config_dir
        .join("identities/team")
        .display()
        .to_string();
    assert_eq!(
        config.prompts_dir_path(&paths),
        paths.data_dir.join("scripts/personas")
    );
    assert_eq!(
        config.identities_dir_path(&paths),
        paths.data_dir.join("identities/team")
    );

    config.prompt.user_identity_file = "./user-identity.md".to_string();
    config.system_prompt_file = Some("./system-prompt.md".to_string());
    assert_eq!(
        config.user_identity_path(&paths),
        paths.data_dir.join("identities/user-identity.md")
    );
    assert_eq!(
        config.system_prompt_path(&paths),
        paths.data_dir.join("prompts/system-prompt.md")
    );

    config.prompt.user_identity_file = paths
        .config_dir
        .join("user-identity.md")
        .display()
        .to_string();
    config.system_prompt_file = Some(
        paths
            .config_dir
            .join("system-prompt.md")
            .display()
            .to_string(),
    );
    assert_eq!(
        config.user_identity_path(&paths),
        paths.data_dir.join("identities/user-identity.md")
    );
    assert_eq!(
        config.system_prompt_path(&paths),
        paths.data_dir.join("prompts/system-prompt.md")
    );

    config.prompt.prompts_dir = "custom-prompts".to_string();
    config.prompt.identities_dir = "custom-identities".to_string();
    config.prompt.user_identity_file = "custom-user.md".to_string();
    config.system_prompt_file = Some("custom-system.md".to_string());
    assert_eq!(
        config.prompts_dir_path(&paths),
        paths.config_dir.join("custom-prompts")
    );
    assert_eq!(
        config.identities_dir_path(&paths),
        paths.config_dir.join("custom-identities")
    );
    assert_eq!(
        config.user_identity_path(&paths),
        paths.config_dir.join("custom-user.md")
    );
    assert_eq!(
        config.system_prompt_path(&paths),
        paths.config_dir.join("custom-system.md")
    );

    let mut deferred_paths = paths.clone();
    deferred_paths.skills_dir = deferred_paths.config_dir.join("skills");
    deferred_paths.scripts_dir = deferred_paths.config_dir.join("scripts");
    let deferred = AppConfig::default();
    assert_eq!(
        deferred.user_identity_path(&deferred_paths),
        deferred_paths.config_dir.join("user-identity.md")
    );
    assert_eq!(
        deferred.system_prompt_path(&deferred_paths),
        deferred_paths.config_dir.join("system-prompt.md")
    );

    let base = directories::BaseDirs::new().unwrap();
    let root = base.home_dir().join(".miyu");
    let mut legacy_paths = paths.clone();
    legacy_paths.config_dir = root.join("config");
    legacy_paths.config_file = root.join("config/config.jsonc");
    legacy_paths.data_dir = root.join("data");
    legacy_paths.skills_dir = root.join("data/skills");
    legacy_paths.scripts_dir = root.join("data/scripts");
    let mut legacy_absolute = AppConfig::default();
    legacy_absolute.prompt.user_identity_file = base
        .config_dir()
        .join("miyu/user-identity.md")
        .display()
        .to_string();
    legacy_absolute.system_prompt_file = Some(
        base.config_dir()
            .join("miyu/system-prompt.md")
            .display()
            .to_string(),
    );
    assert_eq!(
        legacy_absolute.user_identity_path(&legacy_paths),
        root.join("data/identities/user-identity.md")
    );
    assert_eq!(
        legacy_absolute.system_prompt_path(&legacy_paths),
        root.join("data/prompts/system-prompt.md")
    );
}

#[test]
fn reserved_system_prompt_file_is_not_a_persona() {
    let temp = tempfile::tempdir().unwrap();
    let paths = MiyuPaths {
        root_dir: temp.path().to_path_buf(),
        config_dir: temp.path().join("config"),
        config_file: temp.path().join("config/config.jsonc"),
        skills_dir: temp.path().join("data/skills"),
        data_dir: temp.path().join("data"),
        cache_dir: temp.path().join("cache"),
        state_dir: temp.path().join("state"),
        pictures_dir: temp.path().join("data/pictures"),
        fish_hook_file: temp.path().join("fish/miyu.fish"),
        bash_hook_file: temp.path().join("config/shell/bash-hook.sh"),
        zsh_hook_file: temp.path().join("config/shell/zsh-hook.zsh"),
        scripts_dir: temp.path().join("data/scripts"),
        system_scripts_dir: PathBuf::new(),
    };
    std::fs::create_dir_all(paths.prompts_dir()).unwrap();
    std::fs::write(paths.prompts_dir().join("system-prompt.md"), "fallback").unwrap();
    std::fs::write(paths.prompts_dir().join("System Prompt.md"), "persona").unwrap();
    let mut config = AppConfig::default();
    assert!(config.validate_persona_files(&paths).is_ok());
    config.prompt.active_persona = "system-prompt.md".to_string();
    assert!(config.validate_persona_files(&paths).is_err());
}

/// 家目录布局(阶段 6):属主档案与身份文件跟进 home/<admin>,人格记忆去 personas/。
#[test]
fn home_layout_marker_redirects_identity_and_persona_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
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
    std::fs::write(root.join(".home-layout-v1"), "shorin\n").unwrap();
    let mut config = AppConfig::default();
    let home = root.join("home/shorin");
    assert_eq!(config.user_identity_path(&paths), home.join("profile.md"));
    assert_eq!(config.identities_dir_path(&paths), home.join("identities"));
    assert_eq!(
        config.identity_path(&paths, "team/user.md"),
        home.join("identities/team/user.md")
    );
    assert_eq!(
        config.persona_memory_data_dir(&paths, "default"),
        root.join("personas/default")
    );
    assert_eq!(config.prompts_dir_path(&paths), root.join("data/prompts"));
    assert_eq!(
        config.persona_skills_dir(&paths, "team"),
        root.join("extensions/skills/personas/team")
    );
    // 成员回合把 user_identity_file 指到自己家目录的绝对路径:原样采用
    config.prompt.user_identity_file = root.join("home/alice/profile.md").display().to_string();
    assert_eq!(
        config.user_identity_path(&paths),
        root.join("home/alice/profile.md")
    );
    std::fs::create_dir_all(root.join("home/alice")).unwrap();
    std::fs::write(root.join("home/alice/profile.md"), "call me Alice").unwrap();
    assert_eq!(
        config.user_identity_prompt(&paths).unwrap(),
        "call me Alice"
    );
}
