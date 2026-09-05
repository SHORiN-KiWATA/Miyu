//! 内置脚本头部 = 迁移前 index.json 的契约(09-05 迁移)。
//!
//! 内置脚本的描述/参数/超时/分组从 index.json 搬进了各脚本头部,index.json 删除。
//! 夹具是迁移前那份 index,这里逐条比对:参数 schema(去掉 description 文案后)
//! 逐字节相同,超时、分组、显示名不变;描述换成英文后首句 ≤60 字符。

use crate::tools::scripts::*;

const LEGACY_INDEX: &str = include_str!("fixtures/bundled-index-2026-09-05.json");

fn bundled_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/scripts/personas/default")
}

fn strip_descriptions(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("description");
            for nested in map.values_mut() {
                strip_descriptions(nested);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_descriptions),
        _ => {}
    }
}

fn first_sentence_chars(text: &str) -> usize {
    text.split_inclusive(['.', '!', '?'])
        .next()
        .unwrap_or(text)
        .trim()
        .chars()
        .count()
}

#[test]
fn bundled_headers_match_the_legacy_index_contracts() {
    let dir = bundled_dir();
    let scan = scan_scripts(&[dir.as_path()]).unwrap();
    assert!(scan.unregistered.is_empty(), "{:?}", scan.unregistered);
    assert!(
        !dir.join("index.json").exists(),
        "内置目录不该再有 index.json"
    );

    let legacy: ScriptIndex = serde_json::from_str(LEGACY_INDEX).unwrap();
    assert_eq!(legacy.scripts.len(), 8);
    for old in legacy.scripts {
        let new = scan
            .entries
            .iter()
            .find(|entry| entry.id == old.id)
            .unwrap_or_else(|| panic!("{} missing from header scan", old.id));
        let mut old_params = old.parameters.clone();
        strip_descriptions(&mut old_params);
        let mut new_params = new.parameters.clone();
        strip_descriptions(&mut new_params);
        assert_eq!(old_params, new_params, "{}: parameters drifted", old.id);
        assert_eq!(old.timeout_seconds, new.timeout_seconds, "{}", old.id);
        assert_eq!(old.groups, new.groups, "{}", old.id);
        assert!(matches!(new.load_policy, LoadPolicy::Group), "{}", old.id);
        assert_eq!(new.always_loaded, None, "{}", old.id);
        assert_eq!(new.display_name, old.display_name, "{}", old.id);
    }
}

#[test]
fn bundled_descriptions_follow_the_header_style_rules() {
    let scan = scan_scripts(&[bundled_dir().as_path()]).unwrap();
    let ids: Vec<&str> = scan.entries.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "bangumi",
            "battery_care",
            "bilibili_live_stream",
            "crack_search",
            "flight_deals",
            "goofish_search",
            "hotel_deals",
            "procusage",
            "showenv",
            "xhs_search",
            "zhihu_search",
        ]
    );
    for entry in &scan.entries {
        assert!(
            entry
                .description
                .starts_with(|character: char| character.is_ascii_alphabetic()),
            "{}: description must be English: {}",
            entry.id,
            entry.description
        );
        assert!(
            first_sentence_chars(&entry.description) <= 60,
            "{}: first sentence over 60 chars: {}",
            entry.id,
            entry.description
        );
        if let Some(properties) = entry
            .parameters
            .get("properties")
            .and_then(Value::as_object)
        {
            for (name, property) in properties {
                let description = property
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                assert!(
                    description.starts_with(|character: char| character.is_ascii_alphabetic()),
                    "{}.{name}: parameter description must be English: {description}",
                    entry.id
                );
            }
        }
    }
}
