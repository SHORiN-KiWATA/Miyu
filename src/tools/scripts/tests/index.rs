//! 索引扫描、ID 校验与头部/索引合并。

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
fn auto_detects_executable_script() {
    let temp = tempfile::tempdir().unwrap();
    let script_path = temp.path().join("hello.sh");
    std::fs::write(
        &script_path,
        "#!/bin/bash\n# description: Say hello\n\necho hello",
    )
    .unwrap();
    let entry = auto_detect_script(&script_path).unwrap();
    assert_eq!(entry.id, "hello");
    assert_eq!(entry.display_name, "hello");
    assert_eq!(entry.description, "Say hello");
    assert_eq!(entry.path, "hello.sh");
    assert!(entry.parameters.is_null());
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

/// 文件名里的连字符折成下划线:工具名两套规则(自动检测 vs manage_script)统一。
#[test]
fn auto_detect_uses_script_display_name_and_normalized_id() {
    let temp = tempfile::tempdir().unwrap();
    let script_path = temp.path().join("battery-care.sh");
    std::fs::write(
        &script_path,
        "#!/bin/bash\n# 显示名称：电池护理\n# 描述：管理电池充电阈值\n\necho ok",
    )
    .unwrap();
    let entry = auto_detect_script(&script_path).unwrap();
    assert_eq!(entry.id, "battery_care");
    assert_eq!(entry.display_name, "电池护理");
    assert_eq!(entry.description, "管理电池充电阈值");
}

#[test]
fn scan_finds_auto_detected_scripts() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("greet.sh"),
        "#!/bin/bash\n# description: Greet user\n\necho hi",
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
        "#!/bin/bash\n# description: Auto detected\n\necho auto",
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

/// index 是覆盖层:写了的字段压住头部,没写的从头部补。
#[test]
fn index_overrides_header_fields_and_header_fills_the_rest() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("lookup.py"),
        "#!/usr/bin/env python3\n\
# Display name: 头部名\n\
# Description: Header description\n\
# Timeout: 60\n\
# Group: research\n\
# Argv: flags\n\
# Parameters: {\"type\":\"object\",\"properties\":{\"q\":{\"type\":\"string\"}}}\n\
print(1)\n",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("index.json"),
        serde_json::to_string(&json!({
            "scripts": [{
                "id": "lookup",
                "path": "lookup.py",
                "description": "Index description",
                "timeout_seconds": 10
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert_eq!(scan.entries.len(), 1);
    let entry = &scan.entries[0];
    assert_eq!(entry.description, "Index description");
    assert_eq!(entry.timeout_seconds, Some(10));
    assert_eq!(entry.display_name, "头部名");
    assert_eq!(entry.parameters["properties"]["q"]["type"], "string");
    assert_eq!(entry.groups, vec!["research"]);
    assert_eq!(entry.argv, ArgvMode::Flags);

    let spec = entry_to_spec(entry, scripts_dir, scripts_dir).unwrap();
    assert!(!spec.always_loaded, "有 schema 的脚本默认懒加载");
    assert!(
        matches!(spec.load_policy, LoadPolicy::Group),
        "头部给了分组就走 group 目录"
    );
    assert_eq!(spec.groups, vec!["research"]);
}

#[test]
fn header_parameters_make_auto_detected_script_carry_a_schema() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("tool.sh"),
        "#!/bin/sh\n# Description: Tool\n# Parameters:\n# {\"type\":\"object\",\n#  \"properties\":{\"n\":{\"type\":\"integer\"}}}\necho\n",
    )
    .unwrap();
    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(
        scan.entries[0].parameters["properties"]["n"]["type"],
        "integer"
    );
}

#[test]
fn pure_non_ascii_file_name_is_listed_as_unregistered() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("查天气"),
        "#!/bin/sh\n# Description: weather\necho\n",
    )
    .unwrap();
    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert!(scan.entries.is_empty());
    assert_eq!(scan.unregistered.len(), 1);
    assert_eq!(scan.unregistered[0].name, "查天气");
}

#[test]
fn scan_deduplicates_by_path() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    let script = scripts_dir.join("dup.sh");
    std::fs::write(&script, "#!/bin/bash\n# description: Dup\n\necho dup").unwrap();
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
        "#!/bin/bash\n# description: System version\n\necho sys",
    )
    .unwrap();
    std::fs::write(
        user_temp.path().join("tool.sh"),
        "#!/bin/bash\n# description: User version\n\necho user",
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

/// 09-05 起脚本一律默认懒加载;index 里显式 always_loaded:true 才进顶层。
#[test]
fn scripts_default_to_lazy_and_index_always_loaded_forces_top_level() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("generic.sh"),
        "#!/bin/bash\n# Description: Generic script\n\necho generic",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("pinned.sh"),
        "#!/bin/bash\n# Description: Pinned script\n\necho pinned",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("index.json"),
        serde_json::to_string(&json!({
            "scripts": [
                { "id": "generic_script", "path": "generic.sh" },
                { "id": "pinned_script", "path": "pinned.sh", "always_loaded": true }
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
    assert!(!names.contains("generic_script"));
    assert!(names.contains("pinned_script"));
    let load_tools = definitions
        .iter()
        .find(|definition| definition.function.name == "load_tools")
        .unwrap();
    assert!(load_tools
        .function
        .description
        .contains("<available_load_targets>"));
    assert!(load_tools.function.description.contains("generic_script"));
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

/// 四层扫描根:内置(system)与全局(data)各含「顶层 + personas/<人格>」。
/// 内置脚本装在 `<system>/personas/default/`——默认人格解析到该目录,自定义
/// 人格解析到不存在的 `<system>/personas/alter`,天然拿不到内置(隐式门,
/// 09-01)。覆盖顺序低→高:内置平台 < 内置人格 < 全局 < 全局人格。
#[test]
fn script_scan_roots_resolve_persona_substructure_per_layer() {
    let temp = tempfile::tempdir().unwrap();
    let mut paths = crate::paths::MiyuPaths::new().unwrap();
    paths.system_scripts_dir = temp.path().join("system");
    paths.scripts_dir = temp.path().join("data/scripts");

    let default_config = crate::config::AppConfig::default();
    let roots = script_scan_roots(&default_config, &paths);
    assert_eq!(
        roots,
        vec![
            paths.system_scripts_dir.clone(),
            paths.system_scripts_dir.join("personas/default"),
            paths.scripts_dir.clone(),
            paths.scripts_dir.join("personas/default"),
        ],
        "默认人格:四层,内置人格层解析到 personas/default"
    );

    let mut custom = crate::config::AppConfig::default();
    custom.prompt.active_persona = "alter".to_string();
    let custom_roots = script_scan_roots(&custom, &paths);
    assert_eq!(
        custom_roots,
        vec![
            paths.system_scripts_dir.clone(),
            paths.system_scripts_dir.join("personas/alter"),
            paths.scripts_dir.clone(),
            paths.scripts_dir.join("personas/alter"),
        ],
        "自定义人格:内置人格层指向不存在的 personas/alter,扫不到内置"
    );
    // 顶层(平台)两层无论人格都在;差异只在 personas/<人格> 这一维。
    assert_eq!(roots[0], custom_roots[0]);
    assert_eq!(roots[2], custom_roots[2]);
    assert_ne!(roots[1], custom_roots[1]);
}

/// 用户机器实查(09-05):`gpustoggle.bak`(无描述头)与 index 里的 gpustoggle
/// 同 stem,旧扫描把正主从 entries 里删掉、塞进未注册清单。
#[test]
fn backup_sibling_does_not_hide_the_indexed_script() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(scripts_dir.join("gpustoggle"), "#!/bin/bash\necho real\n").unwrap();
    std::fs::write(
        scripts_dir.join("gpustoggle.bak"),
        "#!/bin/bash\necho old\n",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("gpustoggle.orig"),
        "#!/bin/bash\n# Description: stale copy\necho older\n",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("index.json"),
        serde_json::to_string(&json!({
            "scripts": [{
                "id": "gpustoggle",
                "description": "Toggle the GPU",
                "path": "gpustoggle",
                "groups": ["vfio"],
                "load_policy": "summary"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(scan.entries[0].description, "Toggle the GPU");
    assert!(scan.unregistered.is_empty(), "{:?}", scan.unregistered);
    // index 自己写的 groups+summary 组合不被改成 group。
    let spec = entry_to_spec(&scan.entries[0], scripts_dir, scripts_dir).unwrap();
    assert!(matches!(spec.load_policy, LoadPolicy::Summary));
}

/// 同目录里一个同 stem 的别名文件有描述头时,同样不能顶掉 index 正主。
#[test]
fn same_stem_sibling_with_header_does_not_replace_indexed_entry() {
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path();
    std::fs::write(
        scripts_dir.join("tool.py"),
        "#!/usr/bin/env python3\nprint(1)\n",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("tool.sh"),
        "#!/bin/bash\n# Description: shell twin\necho\n",
    )
    .unwrap();
    std::fs::write(
        scripts_dir.join("index.json"),
        r#"{"scripts":[{"id":"tool","description":"Python one","path":"tool.py"}]}"#,
    )
    .unwrap();
    let scan = scan_scripts(&[scripts_dir]).unwrap();
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(scan.entries[0].description, "Python one");
    assert!(scan.entries[0].path.ends_with("tool.py"));
}

/// 头部的五个清单字段一路落到 ToolSpec:信任位、权限、桩示例、指路句、前置工具;
/// index 里显式写的仍是覆盖层。
#[test]
fn manifest_fields_reach_the_tool_spec() {
    let raw = "#!/bin/sh\n\
# Description: Weather lookup\n\
# Trust: external\n\
# Permission: read-only\n\
# Example: {\"city\":\"Tokyo\"}\n\
# Hint: web_fetch: Fetch the page with web_fetch.\n\
# Requires: check_issue\n\
echo ok";
    let metadata = extract_metadata(raw);
    let mut entry = ScriptEntry::overlay("weather".to_string(), "weather".to_string());
    merge_header_defaults(&mut entry, &metadata);
    let spec = entry_to_spec(&entry, Path::new("."), Path::new(".")).unwrap();
    assert_eq!(spec.trust, crate::tools::ToolTrust::External);
    assert_eq!(spec.permission, crate::tools::ToolPermission::ReadOnly);
    assert_eq!(spec.stub_example.as_deref(), Some("{\"city\":\"Tokyo\"}"));
    assert_eq!(
        spec.cross_hints,
        vec![(
            "web_fetch".to_string(),
            " Fetch the page with web_fetch.".to_string()
        )]
    );
    assert_eq!(spec.requires_prior, vec!["check_issue".to_string()]);

    // 缺省:只给属主、writes、无示例。
    let mut plain = ScriptEntry::overlay("plain".to_string(), "plain".to_string());
    plain.description = "Plain".to_string();
    let spec = entry_to_spec(&plain, Path::new("."), Path::new(".")).unwrap();
    assert_eq!(spec.trust, crate::tools::ToolTrust::Owner);
    assert_eq!(spec.permission, crate::tools::ToolPermission::Writes);
    assert!(spec.stub_example.is_none());

    // index 覆盖层写了 trust 就以 index 为准。
    let mut pinned = ScriptEntry::overlay("pinned".to_string(), "pinned".to_string());
    pinned.trust = crate::tools::ToolTrust::External;
    merge_header_defaults(
        &mut pinned,
        &extract_metadata("#!/bin/sh\n# Description: x\n# Trust: owner\necho"),
    );
    assert_eq!(pinned.trust, crate::tools::ToolTrust::External);
}
