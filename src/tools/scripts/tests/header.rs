//! 头部元数据解析:键识别、多行 Parameters 块、跳过无关注释行、id 归一化。

use crate::tools::scripts::*;

#[test]
fn extracts_description_from_shebang_script() {
    let raw = "#!/bin/bash\n# description: Check system status\n\necho ok";
    assert_eq!(
        extract_description(raw),
        Some("Check system status".to_string())
    );
}

#[test]
fn extracts_chinese_description() {
    let raw = "#!/usr/bin/env python3\n# 功能介绍: 检查系统状态\n\nprint('ok')";
    assert_eq!(extract_description(raw), Some("检查系统状态".to_string()));
}

#[test]
fn extracts_bilingual_script_descriptions() {
    let raw = "#!/bin/bash\n# 描述：管理电池\n# Description: Manage the battery\n\necho ok";
    let metadata = extract_metadata(raw);
    assert_eq!(metadata.descriptions.zh, Some("管理电池".to_string()));
    assert_eq!(
        metadata.descriptions.en,
        Some("Manage the battery".to_string())
    );
    // 模型面恒英文:两种都有时选英文。
    assert_eq!(
        extract_description(raw),
        Some("Manage the battery".to_string())
    );
}

#[test]
fn returns_none_when_no_description() {
    let raw = "#!/bin/bash\necho hello";
    assert_eq!(extract_description(raw), None);
}

/// 内置五个 python 脚本第二行都是 coding 声明,老解析器在那一行就断了。
#[test]
fn unknown_comment_lines_do_not_end_the_header() {
    let raw = "#!/usr/bin/env python3\n# -*- coding: utf-8 -*-\n# Copyright: nobody\n# 显示名称：小红书搜索\n# Description: Search notes\n\nimport sys";
    let metadata = extract_metadata(raw);
    assert_eq!(metadata.display_names.zh, Some("小红书搜索".to_string()));
    assert_eq!(metadata.descriptions.en, Some("Search notes".to_string()));
}

#[test]
fn header_ends_at_first_code_line() {
    let raw = "#!/bin/bash\n# Description: Real\nset -e\n# Description: Not header\n";
    assert_eq!(extract_description(raw), Some("Real".to_string()));
}

#[test]
fn parses_multiline_parameters_block_and_scalars() {
    let raw = "#!/usr/bin/env python3\n\
# Description: Lookup tool\n\
# Timeout: 60s\n\
# Group: research, shopping\n\
# Argv: flags\n\
# Parameters:\n\
# {\n\
#   \"type\": \"object\",\n\
#   \"properties\": {\"query\": {\"type\": \"string\"}},\n\
#   \"required\": [\"query\"]\n\
# }\n\
# Display name: Lookup\n\
import sys\n";
    let metadata = extract_metadata(raw);
    assert_eq!(metadata.timeout_seconds, Some(60));
    assert_eq!(metadata.groups, vec!["research", "shopping"]);
    assert_eq!(metadata.argv, Some(ArgvMode::Flags));
    let parameters = metadata.parameters.expect("parameters block parsed");
    assert_eq!(parameters["properties"]["query"]["type"], "string");
    assert_eq!(parameters["required"][0], "query");
    // 块结束后头部继续解析。
    assert_eq!(metadata.display_names.en, Some("Lookup".to_string()));
}

#[test]
fn parses_inline_parameters() {
    let raw = "#!/bin/sh\n# Description: Inline\n# Parameters: {\"type\":\"object\",\"properties\":{\"n\":{\"type\":\"integer\"}}}\necho\n";
    let metadata = extract_metadata(raw);
    assert_eq!(
        metadata.parameters.unwrap()["properties"]["n"]["type"],
        "integer"
    );
}

#[test]
fn accepts_double_slash_comments() {
    let raw = "#!/usr/bin/env node\n// Description: Node tool\n// Timeout: 30\nconsole.log(1)\n";
    let metadata = extract_metadata(raw);
    assert_eq!(metadata.descriptions.en, Some("Node tool".to_string()));
    assert_eq!(metadata.timeout_seconds, Some(30));
}

/// 全角键 + 值里带半角冒号:必须在最先出现的冒号处切,不然键变成「描述：走 stdin」。
#[test]
fn full_width_colon_key_with_half_width_colon_in_value() {
    let raw = "#!/bin/bash\n# 描述：走 stdin: 喂 JSON\necho\n";
    let metadata = extract_metadata(raw);
    assert_eq!(
        metadata.descriptions.zh,
        Some("走 stdin: 喂 JSON".to_string())
    );
}

#[test]
fn normalizes_file_stems_into_tool_ids() {
    assert_eq!(
        normalize_script_id("battery-care"),
        Some("battery_care".to_string())
    );
    assert_eq!(
        normalize_script_id("xhs.search v2"),
        Some("xhs_search_v2".to_string())
    );
    assert_eq!(normalize_script_id("2048"), Some("script_2048".to_string()));
    assert_eq!(normalize_script_id("hello"), Some("hello".to_string()));
    assert_eq!(normalize_script_id("查天气"), None);
}

#[test]
fn header_id_overrides_file_stem_when_valid() {
    let temp = tempfile::tempdir().unwrap();
    let good = temp.path().join("weird-name.sh");
    std::fs::write(&good, "#!/bin/bash\n# Id: nice_name\n# Description: x\n").unwrap();
    assert_eq!(
        inspect_script(&good).unwrap().id,
        Some("nice_name".to_string())
    );

    let bad = temp.path().join("other-name.sh");
    std::fs::write(&bad, "#!/bin/bash\n# Id: bad-name\n# Description: x\n").unwrap();
    assert_eq!(
        inspect_script(&bad).unwrap().id,
        Some("other_name".to_string())
    );
}

#[test]
fn read_header_only_takes_the_first_32kb() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("big.sh");
    let mut body = String::from("#!/bin/bash\n# Description: Big\n");
    body.push_str(&"x".repeat(HEADER_READ_LIMIT * 2));
    std::fs::write(&path, body).unwrap();
    let raw = read_header(&path).unwrap();
    assert_eq!(raw.len(), HEADER_READ_LIMIT);
    assert_eq!(description_from_script(&path), Some("Big".to_string()));
}
