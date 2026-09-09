//! 内容解析与字体回退。

use super::shared::*;
use crate::platforms::plugins::renderer::*;

/// 把一份 markdown 里所有可见文字摊平成一个字符串。表格的文字不在 `spans`
/// 里而在 `table` 里,两处都要走到。
fn flattened_text(markdown: &str) -> String {
    let mut out = String::new();
    for block in collect_blocks(markdown) {
        for span in &block.spans {
            out.push_str(&span.text);
        }
        let Some(table) = block.table.as_ref() else {
            continue;
        };
        for cell in table.header.iter().chain(table.rows.iter().flatten()) {
            for span in cell {
                out.push_str(&span.text);
            }
        }
    }
    out
}

#[test]
fn markdown_links_keep_their_url_visible() {
    // 图里点不了也复制不了,链接被 pulldown-cmark 吃掉之后读者根本不知道它指向哪。
    let text = flattened_text("详见 [Miyu 主页](https://github.com/shorinkiwata/Miyu) 谢谢。");
    assert!(text.contains("Miyu 主页"), "{text}");
    assert!(
        text.contains("(https://github.com/shorinkiwata/Miyu)"),
        "链接必须出现在正文里: {text}"
    );
}

#[test]
fn autolinks_do_not_print_the_url_twice() {
    let text = flattened_text("[https://example.com](https://example.com)");
    assert_eq!(
        text.matches("https://example.com").count(),
        1,
        "标题本身就是网址时不该再补一遍: {text}"
    );
    let bare = flattened_text("<https://example.com/>");
    assert_eq!(bare.matches("example.com").count(), 1, "{bare}");
}

#[test]
fn image_urls_survive_too() {
    // 图渲染器画不出图片,地址一丢读者就完全够不到那张图。
    let text = flattened_text("![群里那张截图](https://example.com/shot.png)");
    assert!(text.contains("群里那张截图"), "{text}");
    assert!(text.contains("(https://example.com/shot.png)"), "{text}");
}

#[test]
fn images_without_alt_text_still_show_their_url() {
    // `![](url)` 以前整块什么都不剩。
    let text = flattened_text("![](https://example.com/bare.png)");
    assert_eq!(text.trim(), "(https://example.com/bare.png)", "{text}");
}

#[test]
fn an_image_wrapped_in_a_link_keeps_both_targets() {
    let text =
        flattened_text("[![缩略图](https://example.com/thumb.png)](https://example.com/full)");
    assert!(text.contains("(https://example.com/thumb.png)"), "{text}");
    assert!(text.contains("(https://example.com/full)"), "{text}");
}

#[test]
fn links_inside_tables_carry_their_url_too() {
    let text =
        flattened_text("| 名字 | 地址 |\n| --- | --- |\n| [首页](https://a.example) | x |\n");
    assert!(text.contains("(https://a.example)"), "{text}");
}

#[test]
fn renderer_client_and_payloads_satisfy_async_bounds() {
    fn assert_send_static<T: Send + 'static>() {}
    fn assert_renderer<T: Clone + Send + Sync + 'static>() {}
    assert_send_static::<RenderConfig>();
    assert_send_static::<RenderedImage>();
    assert_renderer::<MarkdownImageRenderer>();
}

#[test]
fn renderer_client_is_lazy_and_limits_are_bounded() {
    let renderer = MarkdownImageRenderer::new().unwrap();
    assert!(renderer.worker.try_lock().unwrap().process.is_none());
    assert_eq!(MAX_CACHED_GLYPHS, 2048);
    assert_eq!(WORKER_IDLE_TIMEOUT, Duration::from_secs(10 * 60));
    assert_eq!(RENDER_TIMEOUT, Duration::from_secs(60));
    // debug 二进制 550MB+,光映射自身就撞 512MB;开发构建放宽。
    #[cfg(not(debug_assertions))]
    assert_eq!(WORKER_ADDRESS_SPACE_LIMIT, 512 * 1024 * 1024);
    #[cfg(debug_assertions)]
    assert_eq!(WORKER_ADDRESS_SPACE_LIMIT, 2 * 1024 * 1024 * 1024);
}

#[test]
fn renderer_loads_only_the_fonts_needed_by_the_request() {
    let font_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/fonts");
    let mut renderer = RendererState::from_font_dir(&font_dir).unwrap();
    let config = NormalizedConfig::new(&RenderConfig::default());

    let fonts = renderer.resolve_config_fonts(&config, false).unwrap();
    assert!(fonts.emoji.is_none());
    assert!(!renderer.emoji_loaded);
    let cjk_face_count = {
        let faces = renderer.font_system.db().faces().collect::<Vec<_>>();
        assert!(!faces.is_empty());
        // 启动即加载正文 CJK 与等宽代码字体两个文件;emoji 仍按需。
        assert!(faces.iter().all(|face| matches!(
            &face.source,
            fontdb::Source::File(path)
                if path == &font_dir.join(CJK_FONT_FILE)
                    || path == &font_dir.join(CODE_FONT_FILE)
        )));
        let families = faces
            .iter()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.as_str()))
            .collect::<Vec<_>>();
        assert!(families.contains(&DEFAULT_BODY_FONT));
        assert!(families.contains(&DEFAULT_CODE_FONT));
        assert!(!families.contains(&DEFAULT_EMOJI_FONT));
        faces.len()
    };

    let fonts = renderer.resolve_config_fonts(&config, true).unwrap();
    assert_eq!(fonts.emoji.as_deref(), Some(DEFAULT_EMOJI_FONT));
    assert!(renderer.emoji_loaded);
    let with_emoji = renderer.font_system.db().faces().count();
    assert!(with_emoji > cjk_face_count);
    renderer.ensure_bundled_emoji_font().unwrap();
    assert_eq!(renderer.font_system.db().faces().count(), with_emoji);
}

#[test]
fn missing_emoji_font_does_not_block_text_only_requests() {
    let font_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/fonts");
    let mut renderer = RendererState::from_font_dir(&font_dir).unwrap();
    renderer.emoji_font_path = font_dir.join("missing-emoji.ttf");
    let config = NormalizedConfig::new(&RenderConfig::default());

    assert!(renderer.resolve_config_fonts(&config, false).is_ok());
    let error = renderer
        .resolve_config_fonts(&config, true)
        .err()
        .expect("missing Emoji font should fail only Emoji requests");
    assert!(error.to_string().contains("missing-emoji.ttf"));
}

#[test]
fn emoji_detection_only_marks_emoji_graphemes() {
    assert!(!markdown_contains_emoji("纯中文 and `code`"));
    assert!(markdown_contains_emoji("完成 ✅"));
    assert!(markdown_contains_emoji("family 👨‍👩‍👧‍👦"));
}

#[tokio::test]
async fn worker_binary_response_round_trips_without_base64() {
    let expected_png = b"\x89PNG\r\n\x1a\nworker".to_vec();
    let (mut worker_side, mut client_side) = tokio::io::duplex(1024);
    let write = write_worker_response(
        &mut worker_side,
        Ok(vec![RenderedImage {
            mime: "image/png".to_string(),
            png: expected_png.clone(),
            width: 960,
            height: MIN_RENDERED_HEIGHT,
        }]),
    );
    let read = read_worker_response(&mut client_side);
    let (write_result, read_result) = tokio::join!(write, read);
    write_result.unwrap();
    let images = read_result.unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime, "image/png");
    assert_eq!(images[0].png, expected_png);
    assert_eq!(images[0].width, 960);
    assert_eq!(images[0].height, MIN_RENDERED_HEIGHT);
}

#[tokio::test]
async fn request_frames_enforce_the_input_budget() {
    let (mut writer, mut reader) = tokio::io::duplex(64);
    let payload = b"request";
    let (write_result, read_result) = tokio::join!(
        write_frame(&mut writer, payload),
        read_frame(&mut reader, MAX_REQUEST_FRAME_BYTES)
    );
    write_result.unwrap();
    assert_eq!(read_result.unwrap().unwrap(), payload);
    assert!(write_frame(
        &mut tokio::io::sink(),
        &vec![0; MAX_REQUEST_FRAME_BYTES + 1]
    )
    .await
    .is_err());
}

#[test]
fn renders_supported_markdown_and_unicode_to_nonempty_png() {
    let markdown = r#"# Miyu 长回复 🚀

普通中文段落，包含 **粗体**、*斜体*、`inline code` 和 [链接文字](https://example.com)。

> 引用内容支持中文和 Emoji 😀。

- 第一项
- 第二项

1. ordered one
2. ordered two

```rust
fn main() {
println!("hello");
}
```

| 名称 | 状态 |
| --- | --- |
| renderer | ready |

---

结束。"#;
    let pages = render(markdown, &RenderConfig::default()).unwrap();
    assert_eq!(pages.len(), 1);
    for page in pages {
        assert_eq!(page.mime, "image/png");
        assert!(page.png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!((MIN_RENDERED_HEIGHT..=MAX_PAGE_HEIGHT).contains(&page.height));
        assert!(u64::from(page.width) * u64::from(page.height) <= MAX_PAGE_PIXELS);
        let decoded = image::load_from_memory(&page.png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (page.width, page.height));
        let background = decoded.get_pixel(0, 0);
        assert!(decoded.pixels().any(|pixel| pixel != background));
    }
}

#[test]
fn freshly_shaped_cjk_word_keeps_positive_advances() {
    // cosmic-text 0.15 在冷字体系统上首次整词塑形时,"背景"的首字形
    // advance 为 0,后续字形全部叠画在同一位置(0.19 修复)。锁死该回归:
    // 任何字形 advance 归零都会让文字叠加。
    let mut font_system = FontSystem::new();
    for text in ["背景", "背 景", "背包"] {
        let metrics = Metrics::new(36.0, 52.0);
        let mut buffer = Buffer::new(&mut font_system, metrics);
        buffer.set_size(Some(960.0), None);
        let attrs = Attrs::new().family(Family::SansSerif).metrics(metrics);
        buffer.set_rich_text([(text, attrs.clone())], &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut font_system, true);
        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                assert!(
                    glyph.w > 0.0,
                    "{text:?} 中 start={} 的字形 advance 为 0",
                    glyph.start
                );
            }
        }
    }
}

#[test]
fn inline_code_gets_a_background_chip() {
    let count_chip_pixels = |markdown: &str| {
        let pages = render(markdown, &RenderConfig::default()).unwrap();
        let decoded = image::load_from_memory(&pages[0].png).unwrap().to_rgba8();
        let palette = Palette::for_theme(&RenderConfig::default().theme);
        decoded
            .pixels()
            .filter(|pixel| **pixel == Rgba(palette.code_background))
            .count()
    };
    let with_code = count_chip_pixels("行内 `code` 提示,以及 `第二段代码` 也要有底色。");
    let without_code = count_chip_pixels("行内 code 提示,没有任何反引号的对照段落。");
    assert!(
        with_code > 200,
        "行内代码应有底色块,实际命中 {with_code} 像素"
    );
    assert_eq!(without_code, 0, "无行内代码时不应出现底色像素");
}

#[test]
fn input_limit_is_measured_in_unicode_characters() {
    let accepted = "界".repeat(MAX_INPUT_CHARS);
    let rejected = "界".repeat(MAX_INPUT_CHARS + 1);
    assert!(validate_markdown(&accepted).is_ok());
    assert!(validate_markdown(&rejected).is_err());
}

#[test]
fn total_pixel_and_png_writers_enforce_hard_budgets() {
    assert_eq!(
        checked_total_page_pixels(MAX_TOTAL_PAGE_PIXELS - 1, 1).unwrap(),
        MAX_TOTAL_PAGE_PIXELS
    );
    assert!(checked_total_page_pixels(MAX_TOTAL_PAGE_PIXELS, 1).is_err());

    let mut writer = CappedVecWriter::new(3);
    writer.write_all(b"abc").unwrap();
    assert!(writer.write_all(b"d").is_err());
    assert!(writer.exceeded());
    assert_eq!(writer.into_inner(), b"abc");
}

#[test]
fn fenced_configuration_keeps_heading_markers_inside_code() {
    let markdown = r#"下面是 Niri 配置：

```kdl
input {
focus-follows-mouse
keyboard { mod-key "Mod1" }
}
```

Kitty 透明度：

```conf
# ~/.config/kitty/kitty.conf
background_opacity 0.92
dynamic_background_opacity yes
```
"#;
    let blocks = collect_blocks(markdown);
    let code_blocks = blocks
        .iter()
        .filter(|block| block.kind == BlockKind::Code)
        .collect::<Vec<_>>();

    assert_eq!(code_blocks.len(), 2);
    let code_text = code_blocks
        .iter()
        .flat_map(|block| &block.spans)
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert!(code_text.contains("focus-follows-mouse"));
    assert!(code_text.contains("# ~/.config/kitty/kitty.conf"));
    assert!(!blocks.iter().any(|block| {
        matches!(block.kind, BlockKind::Heading(_))
            && block
                .spans
                .iter()
                .any(|span| span.text.contains("kitty.conf"))
    }));
}

#[test]
fn code_surface_remains_distinct_after_qq_sized_downscale() {
    let markdown = r#"正文内容用于对比代码块。

```kdl
# ~/.config/kitty/kitty.conf
background_opacity 0.92
```

代码块之后的正文。"#;
    let raw_config = RenderConfig::default();
    let config = NormalizedConfig::new(&raw_config);
    let palette = Palette::for_theme("paper");
    let mut renderer = RendererState::new().unwrap();
    let fonts = renderer.resolve_config_fonts(&config, false).unwrap();
    let layouts = layout_blocks(
        &mut renderer.font_system,
        collect_blocks(markdown),
        &config,
        palette,
        &fonts,
    )
    .unwrap();
    let code_index = layouts
        .iter()
        .position(|block| block.kind == BlockKind::Code)
        .expect("fenced block should use the code layout");
    let columns = plan_columns(&layouts, &config).unwrap();
    let placement = columns
        .iter()
        .flat_map(|column| &column.placements)
        .find(|placement| placement.block_index == code_index)
        .expect("code block placement");
    let code_y = config.padding + placement.y;
    let sample_y = code_y + (placement.source_end - placement.source_start) / 2;

    let page = render(markdown, &raw_config).unwrap().remove(0);
    let image = image::load_from_memory(&page.png).unwrap().to_rgba8();
    let outside_x = config.padding / 2;
    let inside_x = config.padding + COLUMN_WIDTH - 12;
    assert_eq!(
        *image.get_pixel(outside_x, sample_y),
        Rgba(palette.background)
    );
    assert_eq!(
        *image.get_pixel(inside_x, sample_y),
        Rgba(palette.code_background)
    );
    assert_eq!(
        *image.get_pixel(config.padding, sample_y),
        Rgba(palette.code_background)
    );

    let scaled_width = 568_u32;
    let scaled_height =
        (u64::from(page.height) * u64::from(scaled_width) / u64::from(page.width)).max(1) as u32;
    let scaled = image::imageops::resize(
        &image,
        scaled_width,
        scaled_height,
        image::imageops::FilterType::Triangle,
    );
    let scale_x = |x: u32| (u64::from(x) * u64::from(scaled_width) / u64::from(page.width)) as u32;
    let scale_y =
        |y: u32| (u64::from(y) * u64::from(scaled_height) / u64::from(page.height)) as u32;
    let outside = scaled.get_pixel(scale_x(outside_x), scale_y(sample_y));
    let inside = scaled.get_pixel(scale_x(inside_x), scale_y(sample_y));
    let rgb_distance = (0..3)
        .map(|channel| u32::from(outside[channel].abs_diff(inside[channel])))
        .sum::<u32>();
    assert!(
        rgb_distance > 50,
        "downscaled code surface contrast was only {rgb_distance}"
    );
}

#[test]
fn extreme_config_values_are_clamped_and_missing_fonts_fall_back() {
    let config = RenderConfig {
        theme: "unknown".to_string(),
        max_height: 1,
        font_size: 0,
        code_font_size: u32::MAX,
        padding: u32::MAX,
        font: "/definitely/missing/body.ttf".to_string(),
        title_font: "/definitely/missing/title.ttf".to_string(),
        code_font: "/definitely/missing/code.ttf".to_string(),
        emoji_font: "/definitely/missing/emoji.ttf".to_string(),
    };
    let pages = render("fallback 中文 😀", &config).unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(
        NormalizedConfig::new(&config).max_height,
        MIN_CONFIGURED_HEIGHT
    );
    assert_eq!(pages[0].height, MIN_RENDERED_HEIGHT);
    assert!(!pages[0].png.is_empty());
}

#[test]
fn task_list_markers_are_structured_instead_of_literal_text() {
    let blocks = collect_blocks("- [ ] pending\n- [x] complete\n");
    let tasks = blocks
        .iter()
        .filter_map(|block| block.task.map(|checked| (checked, block)))
        .collect::<Vec<_>>();
    assert_eq!(tasks.len(), 2);
    assert!(!tasks[0].0);
    assert!(tasks[1].0);
    for (_, block) in tasks {
        let text = block
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(!text.contains('•'));
        assert!(!text.contains("[ ]"));
        assert!(!text.contains("[x]"));
    }
}

#[test]
fn checked_task_box_has_drawn_check_while_empty_box_does_not() {
    let background = [255, 255, 255, 255];
    let foreground = [1, 2, 3, 255];
    let mut unchecked = RgbaImage::from_pixel(40, 40, Rgba(background));
    let mut checked = unchecked.clone();
    draw_checkbox(&mut unchecked, 5, 5, 24, false, foreground);
    draw_checkbox(&mut checked, 5, 5, 24, true, foreground);
    assert_eq!(*unchecked.get_pixel(17, 17), Rgba(background));
    assert!(checked
        .enumerate_pixels()
        .any(|(x, y, pixel)| (9..27).contains(&x)
            && (9..27).contains(&y)
            && *pixel == Rgba(foreground)));
}

#[test]
fn documents_over_the_pixel_budget_fail_instead_of_truncating() {
    let mut markdown = String::from("```text\n");
    for _ in 0..500 {
        markdown.push_str("x\n");
    }
    markdown.push_str("```\n");
    let config = RenderConfig {
        max_height: MIN_CONFIGURED_HEIGHT,
        font_size: 56,
        code_font_size: 52,
        padding: 160,
        ..RenderConfig::default()
    };
    let error = render(&markdown, &config).unwrap_err();
    assert!(error.to_string().contains("pixel limit"));
}

/// 表格样张(人工看效果用,不进 CI):cargo test render_table_sample -- --ignored
#[test]
#[ignore]
fn render_table_sample() {
    let markdown = r#"当前记录还是 14 位赞助人,共 17 笔,合计 ¥206.37。名单同步在
[赞助面板](https://github.com/shorinkiwata/Miyu/wiki/sponsors),自动链接
<https://example.com/> 不会被写两遍，图片同理：![榜单截图](https://example.com/board.png)

| 排名 | 赞助人 | QQ | 金额 | 笔数 |
|------|--------|---:|-----:|-----:|
| 1 | [sudo] Password for nobody | 596113920 | ¥49.00 | 1 |
| 2 | 沐风 | 3058704216 | ¥30.00 | 1 |
| 3 | RyanZ | 2077987156 | ¥20.00 | 1 |
| 4 | Yulliil.Moe | 1698752554 | ¥19.83 | 2 |
| 5 | 走啦 | 5429162774 | ¥15.64 | 3 |
| 6 | tythefish v2 | 3819589247 | ¥15.00 | 1 |
| 7 | 换了又 | 3203029400 | ¥15.00 | 1 |
| 8 | 玛丽小姐 | 3504638270 | ¥10.00 | 1 |
| 9 | sudo 原子.fish | 2479343809 | ¥8.00 | 1 |
| 10 | 雾雨凝霜 | 1146112953 | ¥8.00 | 1 |
| 11 | 素萝MISS | 1512013587 | ¥5.20 | 1 |
| 12 | シオフフ | 1531229883 | ¥5.00 | 1 |
| 13 | 回笼觉觉主 | 5297780882 | ¥0.50 | 1 |
"#;
    let pages = render(markdown, &RenderConfig::default()).unwrap();
    let out = std::env::temp_dir().join("miyu-table-sample.png");
    std::fs::write(&out, &pages[0].png).unwrap();
    eprintln!(
        "sample: {} ({}x{})",
        out.display(),
        pages[0].width,
        pages[0].height
    );
}

/// 样张生成器(人工看效果用,不进 CI):cargo test render_font_sample -- --ignored
#[test]
#[ignore]
fn render_font_sample() {
    let markdown = r#"# 字体与圆角样张

正文中文与 English mixed,行内代码 `cargo build --release` 应为等宽圆角小底衬。

```rust
fn main() {
    let answer: u64 = 42; // 等宽对齐检查
    let 中文注释 = "代码块里的中文回退到正文字体";
    println!("iIlL1| oO0 {answer}");
}
```

> 引用块保持原样。

| 列 A | 列 B |
|------|------|
| `x1` | 对齐 |
"#;
    let pages = render(markdown, &RenderConfig::default()).unwrap();
    let out = std::env::temp_dir().join("miyu-font-sample.png");
    std::fs::write(&out, &pages[0].png).unwrap();
    eprintln!(
        "sample: {} ({}x{})",
        out.display(),
        pages[0].width,
        pages[0].height
    );
}
