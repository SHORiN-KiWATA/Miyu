//! 跨工具指路句的唯一出口。
//!
//! 「要看完整日志，用 `read` 按行分页读」这句话本身没错，错的是它写死在 `job`
//! 的常量描述里——而 dev 注册表根本不注册 `read`。描述层按一个更大的母工具集
//! 写，暴露层按模式裁，两边各说各话，模型照着描述去找一个不存在的工具。
//!
//! 所以这类句子不进常量：注册全部结束后到这里来补，被指的工具没注册就一个字
//! 都不加。新增指路句只能加进 `CROSS_TOOL_HINTS`，加错了会被 `cross_hints`
//! 的测试当场抓住（被指工具必须是真实工具名）。
//!
//! 追加是确定性的——同一模式同一份配置，字节恒定，不碰前缀缓存契约
//! （AGENTS §1.1）。`amend_description` 本来就排在 JSON 覆盖之后。

use super::ToolRegistry;

/// `(要改描述的工具, 必须同时在场的工具, 追加的句子)`。
///
/// 句子自带前导空格：它是接在描述末尾的，不是独立一段。
const CROSS_TOOL_HINTS: &[(&str, &str, &str)] = &[
    (
        "job",
        "read",
        " To read a log in full, read its log_path with read (paged by line).",
    ),
    (
        "remember_fact",
        "kb",
        " For knowledge-base documents use the kb tool instead.",
    ),
    (
        "read",
        "kb",
        " Prefix a path with kb: to read the knowledge base instead of the filesystem.",
    ),
    (
        "read",
        "artifact",
        " Prefix a path with artifact: to read the WebUI Artifact workspace (artifact: alone lists it).",
    ),
];

/// 注册表构造收尾时调用一次。内置表之外,脚本/插件也能在清单里自带指路句
/// (`Hint: <tool>: <sentence>` → ToolSpec::cross_hints),同一条规则:被指
/// 工具不在场就不加。
pub(super) fn apply(registry: &mut ToolRegistry) {
    for (tool, requires, suffix) in CROSS_TOOL_HINTS {
        if registry.contains(tool) && registry.contains(requires) {
            registry.amend_description(tool, suffix);
        }
    }
    let mut declared = registry
        .specs()
        .into_iter()
        .filter(|spec| !spec.cross_hints.is_empty())
        .map(|spec| (spec.name.clone(), spec.cross_hints.clone()))
        .collect::<Vec<_>>();
    // HashMap 无序,按名字排定后追加,字节恒定(AGENTS §1.1)。
    declared.sort_by(|a, b| a.0.cmp(&b.0));
    for (tool, hints) in declared {
        for (requires, suffix) in hints {
            if registry.contains(&requires) {
                registry.amend_description(&tool, &suffix);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolRegistry, ToolSpec};

    fn stub(name: &'static str) -> ToolSpec {
        ToolSpec::new(
            name,
            "base.",
            serde_json::json!({"type": "object", "properties": {}}),
            |_args| async move { Ok(String::new()) },
        )
    }

    #[test]
    fn hint_lands_only_when_the_named_tool_is_registered() {
        let mut both = ToolRegistry::new();
        both.register(stub("job"));
        both.register(stub("read"));
        apply(&mut both);
        assert!(both
            .get("job")
            .unwrap()
            .description
            .contains("read its log_path with read"));

        // dev 注册表的形状:有 job 没 read。退回这个提交之前,这里也会带上
        // 那句指路话,而模型照着去调 read 只会撞未知工具。
        let mut job_only = ToolRegistry::new();
        job_only.register(stub("job"));
        apply(&mut job_only);
        assert_eq!(job_only.get("job").unwrap().description, "base.");
    }

    #[test]
    fn hints_only_point_at_real_tools() {
        // 指路句里的目标必须是真实工具名,否则这层机制自己就成了新的悬空引用。
        let known = crate::tools::tool_descriptions::all();
        // 没有 JSON 描述、只在 Rust 里注册的工具,单独列出来。
        let extra = ["job", "load_tools"];
        for (tool, requires, _) in CROSS_TOOL_HINTS {
            for name in [tool, requires] {
                assert!(
                    known.contains_key(*name) || extra.contains(name),
                    "cross-tool hint points at an unknown tool: {name}"
                );
            }
        }
    }

    /// 清单自带的指路句(`Hint: web_fetch: …`)与内置表同一规则:被指工具在场
    /// 才追加,不在场一字不加。
    #[test]
    fn manifest_declared_hints_follow_the_same_presence_rule() {
        let hinted = || {
            stub("weather").with_cross_hints(vec![(
                "web_fetch".to_string(),
                " Fetch the source page with web_fetch.".to_string(),
            )])
        };
        let mut both = ToolRegistry::new();
        both.register(hinted());
        both.register(stub("web_fetch"));
        apply(&mut both);
        assert!(both
            .get("weather")
            .unwrap()
            .description
            .ends_with("base. Fetch the source page with web_fetch."));

        let mut alone = ToolRegistry::new();
        alone.register(hinted());
        apply(&mut alone);
        assert_eq!(alone.get("weather").unwrap().description, "base.");
    }
}
