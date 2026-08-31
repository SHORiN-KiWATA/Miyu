//! 索引扫描、ID 校验与元数据解析。

use crate::tools::scripts::*;

#[test]
fn migrated_script_index_absolute_paths_follow_the_data_directory() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path().join("data/scripts");
    let legacy = temp.path().join("config/scripts/tool.sh");
    assert_eq!(
        resolve_script_path(&legacy.display().to_string(), &scripts_dir),
        scripts_dir.join("tool.sh")
    );
}

#[test]
fn extracts_description_from_shebang_script() {
    let raw = "#!/bin/bash\ndescription: Check system status\n\necho ok";
    assert_eq!(
        extract_description(raw),
        Some("Check system status".to_string())
    );
}

#[test]
fn extracts_chinese_description() {
    let raw = "#!/usr/bin/env python3\n功能介绍: 检查系统状态\n\nprint('ok')";
    assert_eq!(extract_description(raw), Some("检查系统状态".to_string()));
}

#[test]
fn extracts_bilingual_script_descriptions() {
    let raw = "#!/bin/bash\n# 描述： Pacman/AUR安装软件的TUI\n# Description: Pacman/AUR pkg installation TUI\n\necho ok";
    assert_eq!(
        extract_metadata(raw).descriptions,
        ScriptDescriptions {
            zh: Some("Pacman/AUR安装软件的TUI".to_string()),
            en: Some("Pacman/AUR pkg installation TUI".to_string()),
        }
    );
}

#[test]
fn extracts_lowercase_english_description() {
    let raw = "#!/bin/bash\n# description: Pacman/AUR pkg installation TUI\n\necho ok";
    assert_eq!(
        extract_metadata(raw).descriptions,
        ScriptDescriptions {
            zh: None,
            en: Some("Pacman/AUR pkg installation TUI".to_string()),
        }
    );
}

#[test]
fn script_description_falls_back_when_locale_description_missing() {
    let english_only = ScriptDescriptions {
        zh: None,
        en: Some("English only".to_string()),
    };
    assert_eq!(
        select_script_description(&english_only),
        Some("English only".to_string())
    );
}

#[test]
fn returns_none_when_no_description() {
    let raw = "#!/bin/bash\necho hello";
    assert_eq!(extract_description(raw), None);
}

#[test]
fn auto_detects_executable_script() {
    let temp = tempfile::tempdir().unwrap();
    let script_path = temp.path().join("hello.sh");
    std::fs::write(
        &script_path,
        "#!/bin/bash\ndescription: Say hello\n\necho hello",
    )
    .unwrap();
    let entry = auto_detect_script(&script_path).unwrap();
    assert_eq!(entry.id, "hello");
    assert_eq!(entry.display_name, "hello");
    assert_eq!(entry.description, "Say hello");
    assert_eq!(entry.path, "hello.sh");
}

#[test]
fn extracts_script_display_name_metadata() {
    let raw = "#!/bin/bash\n# 显示名称：电池护理\n# 描述：管理电池充电阈值\n\necho ok";
    let metadata = extract_metadata(raw);
    assert_eq!(metadata.display_names.zh, Some("电池护理".to_string()));
    assert_eq!(
        metadata.descriptions.zh,
        Some("管理电池充电阈值".to_string())
    );
}

#[test]
fn auto_detect_uses_script_display_name() {
    let temp = tempfile::tempdir().unwrap();
    let script_path = temp.path().join("battery-care.sh");
    std::fs::write(
        &script_path,
        "#!/bin/bash\n# 显示名称：电池护理\n# 描述：管理电池充电阈值\n\necho ok",
    )
    .unwrap();
    let entry = auto_detect_script(&script_path).unwrap();
    assert_eq!(entry.id, "battery-care");
    assert_eq!(entry.display_name, "电池护理");
    assert_eq!(entry.description, "管理电池充电阈值");
}

#[test]
fn scan_finds_auto_detected_scripts() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("greet.sh"),
        "#!/bin/bash\ndescription: Greet user\n\necho hi",
    )
    .unwrap();
    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(scan.entries[0].id, "greet");
    assert!(scan.unregistered.is_empty());
}

#[test]
fn scan_merges_index_and_auto_detected() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("index.json"),
        r#"{"scripts":[{"id":"custom","display_name":"自定义","description":"Custom tool","path":"custom.sh"}]}"#,
    )
    .unwrap();
    std::fs::write(scripts_dir.join("custom.sh"), "#!/bin/bash\necho custom").unwrap();
    std::fs::write(
        scripts_dir.join("auto.sh"),
        "#!/bin/bash\ndescription: Auto detected\n\necho auto",
    )
    .unwrap();
    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert_eq!(scan.entries.len(), 2);
    let ids: Vec<&str> = scan.entries.iter().map(|e| e.id.as_str()).collect();
    assert!(ids.contains(&"custom"));
    assert!(ids.contains(&"auto"));
}

#[test]
fn scan_fills_empty_index_description_from_script_header() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("index.json"),
        r#"{"scripts":[{"id":"custom","display_name":"自定义","description":"","path":"custom.sh"}]}"#,
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("custom.sh"),
        "#!/bin/bash\n# Description: Custom header description\n\necho custom",
    )
    .unwrap();
    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(scan.entries[0].description, "Custom header description");
}

#[tokio::test]
async fn register_script_uses_header_description_when_omitted() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("pkg.sh"),
        "#!/bin/bash\n# Description: Pacman/AUR pkg installation TUI\n\necho ok",
    )
    .unwrap();

    register_script_handler(
        json!({
            "id": "pkg_install",
            "path": "pkg.sh"
        }),
        scripts_dir,
    )
    .await
    .unwrap();

    let raw = std::fs::read_to_string(scripts_dir.join("index.json")).unwrap();
    let index: ScriptIndex = serde_json::from_str(&raw).unwrap();
    assert_eq!(index.scripts.len(), 1);
    assert_eq!(
        index.scripts[0].description,
        "Pacman/AUR pkg installation TUI"
    );
}

#[test]
fn scan_deduplicates_by_path() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    let script = scripts_dir.join("dup.sh");
    std::fs::write(&script, "#!/bin/bash\ndescription: Dup\n\necho dup").unwrap();
    std::fs::write(
        scripts_dir.join("index.json"),
        r#"{"scripts":[{"id":"alias1","display_name":"A1","description":"alias","path":"dup.sh"}]}"#,
    )
    .unwrap();
    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert_eq!(scan.entries.len(), 1);
}

#[test]
fn scan_user_dir_overrides_system_dir() {
    let sys_temp = tempfile::tempdir().unwrap();
    let user_temp = tempfile::tempdir().unwrap();
    std::fs::write(
        sys_temp.path().join("tool.sh"),
        "#!/bin/bash\ndescription: System version\n\necho sys",
    )
    .unwrap();
    std::fs::write(
        user_temp.path().join("tool.sh"),
        "#!/bin/bash\ndescription: User version\n\necho user",
    )
    .unwrap();
    let scan = scan_scripts(&[sys_temp.path(), user_temp.path()]).unwrap();
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(scan.entries[0].description, "User version");
}

#[test]
fn scan_lists_scripts_without_descriptions_as_unregistered() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(scripts_dir.join("unknown.sh"), "#!/bin/bash\necho unknown").unwrap();

    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert!(scan.entries.is_empty());
    assert_eq!(scan.unregistered.len(), 1);
    assert_eq!(scan.unregistered[0].name, "unknown");
    assert_eq!(
        scan.unregistered[0].path,
        scripts_dir.join("unknown.sh").to_string_lossy()
    );
}

#[test]
fn scan_drives_top_level_and_available_script_visibility() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("generic.sh"),
        "#!/bin/bash\n# Description: Generic script\n\necho generic",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("lazy.sh"),
        "#!/bin/bash\n# Description: Lazy script\n\necho lazy",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("index.json"),
        serde_json::to_string(&json!({
            "scripts": [
                {
                    "id": "generic_script",
                    "display_name": "Generic",
                    "description": "Generic script",
                    "path": "generic.sh"
                },
                {
                    "id": "lazy_script",
                    "display_name": "Lazy",
                    "description": "Lazy script",
                    "path": "lazy.sh",
                    "parameters": {
                        "type": "object",
                        "properties": {"query": {"type": "string"}}
                    }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let scan = scan_scripts(&[scripts_dir]).unwrap();
    let specs = script_specs(&scan.entries, scripts_dir, scripts_dir);
    let mut registry = ToolRegistry::new();
    crate::tools::load_tools::register(&mut registry);
    registry
        .replace_script_tools(specs, scan.unregistered)
        .unwrap();

    let definitions = registry.lazy_definitions(&BTreeSet::new());
    let names = definitions
        .iter()
        .map(|definition| definition.function.name.as_str())
        .collect::<BTreeSet<_>>();
    assert!(names.contains("generic_script"));
    assert!(!names.contains("lazy_script"));
    let load_tools = definitions
        .iter()
        .find(|definition| definition.function.name == "load_tools")
        .unwrap();
    assert!(load_tools
        .function
        .description
        .contains("<available_load_targets>"));
    assert!(load_tools.function.description.contains("lazy_script"));
}

#[tokio::test]
async fn register_rejects_reserved_tool_names_before_writing_index() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("weather.sh"),
        "#!/bin/bash\n# Description: Fake weather\n\necho fake",
    )
    .unwrap();

    let error =
        register_script_handler(json!({"id":"get_weather","path":"weather.sh"}), scripts_dir)
            .await
            .unwrap_err();
    assert!(error.to_string().contains("reserved tool name"));
    assert!(!scripts_dir.join("index.json").exists());
}

#[test]
fn invalid_external_index_entry_does_not_hide_valid_local_scripts() {
    let scripts_temp = tempfile::tempdir().unwrap();
    let external_temp = tempfile::tempdir().unwrap();
    let scripts_dir = scripts_temp.path();
    let external_script = external_temp.path().join("external.sh");
    std::fs::write(
        &external_script,
        "#!/bin/bash\n# Description: External\n\necho external",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("local.sh"),
        "#!/bin/bash\n# Description: Local\n\necho local",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("index.json"),
        serde_json::to_string(&json!({
            "scripts": [{
                "id": "external_script",
                "display_name": "External",
                "description": "External",
                "path": external_script
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(scan.entries[0].id, "local");
}

#[test]
fn malformed_index_entries_do_not_hide_valid_scripts() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("valid.sh"),
        "#!/bin/bash\n# Description: Valid\n\necho valid",
    )
    .unwrap();
    std::fs::write(scripts_dir.join("invalid.sh"), "not a script").unwrap();
    std::fs::write(
        scripts_dir.join("index.json"),
        serde_json::to_string(&json!({
            "scripts": [
                "broken entry",
                {
                    "id": "",
                    "display_name": "Invalid",
                    "description": "Invalid",
                    "path": "invalid.sh"
                },
                {
                    "id": "valid_script",
                    "display_name": "Valid",
                    "description": "Valid",
                    "path": "valid.sh"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(scan.entries[0].id, "valid_script");
}

#[tokio::test]
async fn lifecycle_mutations_replace_and_remove_all_same_id_entries() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("old.sh"),
        "#!/bin/bash\n# Description: Old\n\necho old",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("new.sh"),
        "#!/bin/bash\n# Description: New\n\necho new",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("index.json"),
        serde_json::to_string(&json!({
            "scripts": [
                {"id": "target_script", "path": 42},
                {
                    "id": "target_script",
                    "display_name": "Old",
                    "description": "Old",
                    "path": "old.sh"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    register_script_handler(json!({"id":"target_script","path":"new.sh"}), scripts_dir)
        .await
        .unwrap();

    let index_path = scripts_dir.join("index.json");
    let mut index = read_script_index_value(&index_path).unwrap();
    let scripts = index_array_mut(&mut index, "scripts").unwrap();
    assert_eq!(
        scripts
            .iter()
            .filter(|entry| raw_entry_field(entry, "id") == Some("target_script"))
            .count(),
        1
    );
    assert_eq!(raw_entry_field(&scripts[0], "path"), Some("new.sh"));

    scripts.insert(0, json!({"id": "target_script", "path": 42}));
    write_script_index_value(&index_path, &index).unwrap();
    unregister_script_handler(
        json!({"id":"target_script","delete_file":false}),
        scripts_dir,
    )
    .await
    .unwrap();

    let index = read_script_index_value(&index_path).unwrap();
    let scripts = index.get("scripts").and_then(Value::as_array).unwrap();
    assert!(!scripts
        .iter()
        .any(|entry| raw_entry_field(entry, "id") == Some("target_script")));
    let disabled = index.get("disabled").and_then(Value::as_array).unwrap();
    assert!(disabled.iter().any(|entry| {
        raw_entry_field(entry, "id") == Some("target_script")
            && raw_entry_field(entry, "path") == Some("new.sh")
    }));
}
