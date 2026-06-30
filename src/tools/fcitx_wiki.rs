use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;

const MAX_PAGE_BYTES: usize = 512 * 1024;
const MAX_EXCERPT_CHARS: usize = 12_000;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        "fcitx5_input_method_wiki_qurey",
        "Query official Fcitx 5 Wiki guidance for Linux input method diagnosis. Returns bilingual structured claims from a small official-page whitelist; use after inspect_issue for Fcitx/XIM/GTK/Qt/Wayland questions. This is not general web search.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural language question, e.g. XWayland XIM, Electron input method, GTK_IM_MODULE, LC_CTYPE." },
                "topic": { "type": "string", "enum": ["auto", "home", "for_users", "setup", "wayland", "environment_variables", "xim", "gtk", "qt", "electron_chromium", "locale"], "description": "Focused Fcitx Wiki topic. Defaults to auto." },
                "language": { "type": "string", "enum": ["bilingual", "zh", "en"], "description": "Output language wrapper. Defaults to bilingual." },
                "include_page_excerpt": { "type": "boolean", "description": "Fetch and include a clipped excerpt from the official wiki page. Defaults to false for stable compact output." }
            },
            "required": [],
            "additionalProperties": false
        }),
        |args| async move { query(args).await },
    ));
}

async fn query(args: Value) -> Result<String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let topic = args
        .get("topic")
        .and_then(Value::as_str)
        .unwrap_or("auto")
        .trim();
    let language = parse_language(
        args.get("language")
            .and_then(Value::as_str)
            .unwrap_or("bilingual"),
    );
    let include_page_excerpt = args
        .get("include_page_excerpt")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let topic = select_topic(topic, query)?;
    let page = page_for_topic(topic);
    let claims = claims_for_topic(topic, language);
    let mut response = WikiResponse {
        ok: true,
        tool: "fcitx5_input_method_wiki_qurey",
        spelling_note: "Tool name keeps the requested 'qurey' spelling for compatibility.",
        topic,
        query: (!query.is_empty()).then(|| query.to_string()),
        source: page,
        claims,
        diagnostic_rule: text(
            language,
            "本机运行时证据优先；Wiki 只能提供官方一般规则，不能覆盖 /proc、环境变量、已加载模块或实际输入测试。",
            "Local runtime evidence comes first; the Wiki provides official general rules and must not override /proc data, environment, loaded modules, or an actual input test.",
        ),
        page_excerpt: None,
    };
    if include_page_excerpt {
        response.page_excerpt = Some(fetch_page_excerpt(page.url).await?);
    }
    Ok(match language {
        Language::En => serde_json::to_string_pretty(&response)?,
        Language::Zh | Language::Bilingual => format_zh_wrapped_response(&response, language)?,
    })
}

fn parse_language(value: &str) -> Language {
    match value {
        "zh" => Language::Zh,
        "en" => Language::En,
        _ => Language::Bilingual,
    }
}

fn select_topic(topic: &str, query: &str) -> Result<&'static str> {
    if topic != "auto" && !topic.is_empty() {
        return match topic {
            "home" => Ok("home"),
            "for_users" => Ok("for_users"),
            "setup" => Ok("setup"),
            "wayland" => Ok("wayland"),
            "environment_variables" => Ok("environment_variables"),
            "xim" => Ok("xim"),
            "gtk" => Ok("gtk"),
            "qt" => Ok("qt"),
            "electron_chromium" => Ok("electron_chromium"),
            "locale" => Ok("locale"),
            _ => bail!("unsupported fcitx wiki topic: {topic}"),
        };
    }
    let lower = query.to_ascii_lowercase();
    if lower.contains("xim") || lower.contains("xmodifiers") || lower.contains("xwayland") {
        Ok("xim")
    } else if lower.contains("electron") || lower.contains("chromium") || lower.contains("wechat") {
        Ok("electron_chromium")
    } else if lower.contains("gtk") || lower.contains("gtk_im_module") {
        Ok("gtk")
    } else if lower.contains("qt")
        || lower.contains("qt_im_module")
        || lower.contains("qt_im_modules")
    {
        Ok("qt")
    } else if lower.contains("lc_ctype") || lower.contains("locale") || lower.contains("lang") {
        Ok("locale")
    } else if lower.contains("wayland") || lower.contains("text-input") || lower.contains("ozone") {
        Ok("wayland")
    } else if lower.contains("setup") || lower.contains("install") || lower.contains("配置") {
        Ok("setup")
    } else {
        Ok("for_users")
    }
}

fn page_for_topic(topic: &str) -> WikiPage {
    match topic {
        "home" | "for_users" => WikiPage {
            title: "Fcitx 5",
            url: "https://fcitx-im.org/wiki/Special:MyLanguage/Fcitx_5",
        },
        "setup" => WikiPage {
            title: "Setup Fcitx 5",
            url: "https://fcitx-im.org/wiki/Special:MyLanguage/Setup_Fcitx_5",
        },
        "wayland" | "electron_chromium" => WikiPage {
            title: "Using Fcitx 5 on Wayland",
            url: "https://fcitx-im.org/wiki/Special:MyLanguage/Using_Fcitx_5_on_Wayland",
        },
        "environment_variables" | "xim" | "gtk" | "qt" | "locale" => WikiPage {
            title: "Input method related environment variables",
            url: "https://fcitx-im.org/wiki/Special:MyLanguage/Input_method_related_environment_variables",
        },
        _ => WikiPage {
            title: "Fcitx 5",
            url: "https://fcitx-im.org/wiki/Special:MyLanguage/Fcitx_5",
        },
    }
}

fn claims_for_topic(topic: &str, language: Language) -> Vec<WikiClaim> {
    match topic {
        "home" => vec![claim(
            language,
            "Fcitx 5 是带插件扩展能力的输入法框架，主页的 For Users 区域链接到安装、设置、FAQ、Wayland 使用、技巧和升级页面。",
            "Fcitx 5 is an extensible input method framework; the home page's For Users section links to install, setup, FAQ, Wayland usage, tips, and upgrade pages.",
            "orientation",
            "Use the home page to discover official user-facing pages, not to diagnose one app by itself.",
        )],
        "for_users" => vec![claim(
            language,
            "For Users 不是单独的 User Guide 页面，而是 Fcitx 5 主页上的用户入口列表；诊断时优先跳到 Setup、Wayland、FAQ 或环境变量页面。",
            "For Users is a user-facing link section on the Fcitx 5 home page, not a standalone User Guide page; diagnostics should jump to Setup, Wayland, FAQ, or environment-variable pages.",
            "navigation",
            "The literal User_Guide page is empty, so do not treat it as authoritative content.",
        )],
        "setup" => vec![
            claim(
                language,
                "官方 Setup 页把 `XMODIFIERS=@im=fcitx`、`GTK_IM_MODULE=fcitx`、`QT_IM_MODULE=fcitx` 作为基础环境变量示例，但强调过渡期没有适合所有人的完美方案。",
                "The official Setup page lists `XMODIFIERS=@im=fcitx`, `GTK_IM_MODULE=fcitx`, and `QT_IM_MODULE=fcitx` as basic environment examples, while noting there is no perfect one-size-fits-all setup during the transition period.",
                "setup",
                "Do not conclude a variable is required globally without considering toolkit, backend, compositor, and app packaging.",
            ),
            claim(
                language,
                "Setup 页提到 systemd `environment.d` 可配置会话环境，但变更通常需要重新登录或重启用户会话才生效。",
                "The Setup page mentions systemd `environment.d` for session environment, with changes generally requiring re-login or a restarted user session.",
                "environment",
                "The target process environment is the evidence that matters, not just the current shell.",
            ),
        ],
        "wayland" => vec![
            claim(
                language,
                "Wayland 页明确说 XWayland 下 X11 应用与普通 X11 几乎没有区别，因此仍需要 `XMODIFIERS=@im=fcitx`。",
                "The Wayland page says X11 applications under XWayland are nearly the same as normal X11, so `XMODIFIERS=@im=fcitx` is still needed.",
                "xwayland",
                "This does not prove a specific app calls XIM; it makes XMODIFIERS relevant evidence for X11/XWayland paths.",
            ),
            claim(
                language,
                "现代 GTK3/GTK4 Wayland 应用可走 text-input-v3；理想设置通常不是全局强制 `GTK_IM_MODULE`。",
                "Modern GTK3/GTK4 Wayland applications can use text-input-v3; the ideal setup usually does not globally force `GTK_IM_MODULE`.",
                "gtk-wayland",
                "Legacy, XWayland, compositor-specific, or per-app cases may still need module overrides.",
            ),
        ],
        "xim" => vec![
            claim(
                language,
                "`XMODIFIERS` 只影响 XIM，Fcitx 的常见值是 `@im=fcitx`。",
                "`XMODIFIERS` affects XIM only; Fcitx commonly uses `@im=fcitx`.",
                "xim",
                "The variable is an activation request, not proof that the app uses XIM.",
            ),
            claim(
                language,
                "非 CJK locale 下如果不设置 `XMODIFIERS`，一些应用的 XIM 不会工作；同时 XIM 还要求 locale 有效，不能是 `C` 或 `POSIX`。",
                "In non-CJK locales, XIM may not work for some applications without `XMODIFIERS`; XIM also requires a valid locale and must not use `C` or `POSIX`.",
                "locale",
                "Check `LANG`/`LC_CTYPE` in the target process and confirm the locale exists in `locale -a`.",
            ),
            claim(
                language,
                "Wayland 页说明 X11/XWayland 应用仍按 X11 路径处理，XWayland 本身不应被当作 XIM 失效的证据。",
                "The Wayland page says X11/XWayland applications still follow the X11 path; XWayland itself should not be treated as evidence that XIM cannot work.",
                "xwayland",
                "App/toolkit support and actual behavior still need local evidence.",
            ),
        ],
        "gtk" => vec![
            claim(
                language,
                "`GTK_IM_MODULE` 会覆盖 GTK 的自动输入法模块选择；如果指定模块找不到，GTK 会回退到自动选择。",
                "`GTK_IM_MODULE` overrides GTK's automatic input module selection; if the requested module is not found, GTK falls back to automatic selection.",
                "gtk",
                "Inspect GTK immodule cache and loaded `im-*.so` before claiming a GTK module is active.",
            ),
            claim(
                language,
                "Fcitx 在 GTK immodule 中声明支持 `zh:ja:ko:*`，相关信息记录在 GTK immodule cache 中。",
                "Fcitx declares GTK immodule support for `zh:ja:ko:*`, and this information is recorded in GTK immodule cache files.",
                "gtk-cache",
                "Locale-based selection must be checked against the actual cache in host/runtime/container.",
            ),
        ],
        "qt" => vec![
            claim(
                language,
                "Qt 输入法模块不需要 GTK 那样的 cache；`QT_IM_MODULE` 会覆盖 Qt 默认选择。",
                "Qt input modules do not need GTK-style cache files; `QT_IM_MODULE` overrides Qt's default choice.",
                "qt",
                "Bundled/proprietary Qt apps may lack fcitx/ibus platform input context plugins, so process/package evidence matters.",
            ),
            claim(
                language,
                "Qt 6.7 引入 `QT_IM_MODULES` 作为 fallback 顺序，例如 `wayland;fcitx;ibus`。",
                "Qt 6.7 introduced `QT_IM_MODULES` as a fallback order, for example `wayland;fcitx;ibus`.",
                "qt6",
                "Qt 4/5 may still require `QT_IM_MODULE`, so do not replace all Qt rules with `QT_IM_MODULES`.",
            ),
        ],
        "electron_chromium" => vec![
            claim(
                language,
                "Wayland 页把 XWayland 下的 Electron/Chromium 归入类似 GTK2 的传统路径；不要断言它们一定不看 XIM。",
                "The Wayland page treats Electron/Chromium under XWayland as a traditional path similar to GTK2; do not assert that they categorically ignore XIM.",
                "electron-xwayland",
                "Specific Electron/CEF/AppImage builds may be patched or old; local runtime evidence and actual input behavior win.",
            ),
            claim(
                language,
                "原生 Wayland Chromium/Electron 路径需要 Ozone Wayland 和 Wayland IM 相关 flags；Electron 不支持 Chromium 的 GTK4 路径。",
                "Native Wayland Chromium/Electron paths need Ozone Wayland and Wayland IM flags; Electron does not support Chromium's GTK4 path.",
                "electron-wayland",
                "Only apply this to confirmed native Wayland windows, not mixed `WAYLAND_DISPLAY` + `DISPLAY` environments.",
            ),
        ],
        "locale" => vec![
            claim(
                language,
                "XIM 需要有效 locale；locale 必须出现在 `locale -a`，并且不能是 `C` 或 `POSIX`。",
                "XIM needs a valid locale; the locale must appear in `locale -a` and must not be `C` or `POSIX`.",
                "locale",
                "Check the target process, not only the shell that launched Miyu.",
            ),
            claim(
                language,
                "Wiki 提到某些 XIM 场景可以用 `LC_CTYPE=zh_CN.UTF-8` 作为 workaround，尤其是历史上 Emacs/Java 一类问题。",
                "The Wiki mentions `LC_CTYPE=zh_CN.UTF-8` as a workaround in some XIM cases, historically including Emacs/Java-like issues.",
                "workaround",
                "Treat this as a testable workaround, not a universal root cause.",
            ),
        ],
        _ => Vec::new(),
    }
}

fn claim(
    language: Language,
    zh: &'static str,
    en: &'static str,
    applies_to: &'static str,
    caveat: &'static str,
) -> WikiClaim {
    WikiClaim {
        claim: text(language, zh, en),
        applies_to,
        confidence: "official_wiki_general_rule",
        caveat,
    }
}

fn text(language: Language, zh: &'static str, en: &'static str) -> BilingualText {
    match language {
        Language::Bilingual => BilingualText {
            zh: Some(zh),
            en: Some(en),
        },
        Language::Zh => BilingualText {
            zh: Some(zh),
            en: None,
        },
        Language::En => BilingualText {
            zh: None,
            en: Some(en),
        },
    }
}

async fn fetch_page_excerpt(url: &str) -> Result<String> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent("miyu/0.1 fcitx5_input_method_wiki_qurey")
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?;
    if response.content_length().unwrap_or(0) > MAX_PAGE_BYTES as u64 {
        bail!("Fcitx Wiki page too large")
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_PAGE_BYTES {
        bail!("Fcitx Wiki page too large")
    }
    Ok(clip(&html2md::parse_html(&String::from_utf8_lossy(&bytes))))
}

fn clip(value: &str) -> String {
    if value.chars().count() <= MAX_EXCERPT_CHARS {
        value.to_string()
    } else {
        format!(
            "{}\n...[truncated to {MAX_EXCERPT_CHARS} chars]",
            value.chars().take(MAX_EXCERPT_CHARS).collect::<String>()
        )
    }
}

fn format_zh_wrapped_response(response: &WikiResponse, language: Language) -> Result<String> {
    let claims = response
        .claims
        .iter()
        .map(|item| {
            let mut value = json!({
                "结论": item.claim.zh.unwrap_or_default(),
                "适用范围": item.applies_to,
                "可信度": item.confidence,
                "注意事项": item.caveat,
            });
            if language == Language::Bilingual {
                value["english_reference"] = json!(item.claim.en.unwrap_or_default());
            }
            value
        })
        .collect::<Vec<_>>();
    let mut output = json!({
        "状态": response.ok,
        "工具": response.tool,
        "说明": "这是 Fcitx5 官方 Wiki 专用查询工具；中文为主包装，英文保留为原文参考。",
        "命名备注": "工具名按用户要求保留 qurey 拼写。",
        "主题": response.topic,
        "查询": response.query,
        "来源": {
            "标题": response.source.title,
            "URL": response.source.url,
        },
        "官方规则摘录": claims,
        "诊断使用规则": response.diagnostic_rule.zh.unwrap_or_default(),
        "页面摘录": response.page_excerpt,
    });
    if language == Language::Bilingual {
        output["diagnostic_rule_en"] = json!(response.diagnostic_rule.en.unwrap_or_default());
    }
    Ok(serde_json::to_string_pretty(&output)?)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Language {
    Bilingual,
    Zh,
    En,
}

#[derive(Serialize)]
struct WikiResponse {
    ok: bool,
    tool: &'static str,
    spelling_note: &'static str,
    topic: &'static str,
    query: Option<String>,
    source: WikiPage,
    claims: Vec<WikiClaim>,
    diagnostic_rule: BilingualText,
    page_excerpt: Option<String>,
}

#[derive(Clone, Copy, Serialize)]
struct WikiPage {
    title: &'static str,
    url: &'static str,
}

#[derive(Serialize)]
struct WikiClaim {
    claim: BilingualText,
    applies_to: &'static str,
    confidence: &'static str,
    caveat: &'static str,
}

#[derive(Serialize)]
struct BilingualText {
    #[serde(skip_serializing_if = "Option::is_none")]
    zh: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    en: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_selects_xim_for_xmodifiers_query() {
        assert_eq!(
            select_topic("auto", "wechat XWayland XMODIFIERS").unwrap(),
            "xim"
        );
    }

    #[test]
    fn electron_claim_warns_against_categorical_xim_denial() {
        let claims = claims_for_topic("electron_chromium", Language::Bilingual);

        assert!(claims.iter().any(|item| item
            .claim
            .en
            .is_some_and(|text| text.contains("do not assert"))));
    }

    #[test]
    fn zh_language_omits_english_wrapper() {
        let claims = claims_for_topic("xim", Language::Zh);

        assert!(claims.iter().all(|item| item.claim.zh.is_some()));
        assert!(claims.iter().all(|item| item.claim.en.is_none()));
    }

    #[tokio::test]
    async fn query_returns_bilingual_source_without_network_by_default() {
        let output = query(json!({
            "topic": "electron_chromium",
            "language": "bilingual"
        }))
        .await
        .unwrap();

        assert!(output.contains("Using Fcitx 5 on Wayland"));
        assert!(output.contains("官方规则摘录"));
        assert!(output.contains("不要断言"));
        assert!(output.contains("do not assert"));
        assert!(output.contains("页面摘录"));
        assert!(output.contains("english_reference"));
    }

    #[tokio::test]
    async fn zh_query_uses_chinese_wrapper_without_english_reference() {
        let output = query(json!({
            "topic": "xim",
            "language": "zh"
        }))
        .await
        .unwrap();

        assert!(output.contains("官方规则摘录"));
        assert!(output.contains("诊断使用规则"));
        assert!(!output.contains("english_reference"));
    }
}
