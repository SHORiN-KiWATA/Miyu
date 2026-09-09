//! 分栏、表格与排版。

use super::shared::*;
use crate::platforms::plugins::renderer::*;

#[test]
fn html_only_output_is_not_rendered_as_a_blank_page() {
    let blocks = collect_blocks("<div>visible</div>");
    assert!(blocks
        .iter()
        .any(|block| { block.spans.iter().any(|span| span.text.contains("visible")) }));
}

#[test]
fn empty_markdown_produces_a_valid_blank_page() {
    let pages = render("", &RenderConfig::default()).unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].mime, "image/png");
    assert_eq!(pages[0].height, MIN_RENDERED_HEIGHT);
    assert!(image::load_from_memory(&pages[0].png).is_ok());
}

#[test]
fn table_parser_preserves_cells_rows_and_alignment() {
    let blocks =
        collect_blocks("| Left | Center | Right |\n| :--- | :---: | ---: |\n| a | b | c |\n");
    let table = blocks
        .iter()
        .find_map(|block| block.table.as_ref())
        .expect("structured table");
    assert_eq!(
        table.alignments,
        vec![Alignment::Left, Alignment::Center, Alignment::Right]
    );
    assert_eq!(table.header.len(), 3);
    assert_eq!(table.rows.len(), 1);
    assert_eq!(table.rows[0].len(), 3);
    assert!(table.header[0].iter().all(|span| span.style.bold));
}

#[test]
fn code_block_uses_remaining_column_space_before_continuing() {
    let mut markdown = String::from("```text\n");
    for line in 0..8 {
        markdown.push_str(&format!("first {line}\n"));
    }
    markdown.push_str("```\n\n```text\n");
    for line in 0..12 {
        markdown.push_str(&format!("second {line}\n"));
    }
    markdown.push_str("```\n");

    let config = NormalizedConfig::new(&RenderConfig {
        max_height: MIN_CONFIGURED_HEIGHT,
        ..RenderConfig::default()
    });
    let mut renderer = RendererState::new().unwrap();
    let fonts = renderer.resolve_config_fonts(&config, false).unwrap();
    let layouts = layout_blocks(
        &mut renderer.font_system,
        collect_blocks(&markdown),
        &config,
        Palette::for_theme("paper"),
        &fonts,
    )
    .unwrap();
    assert_eq!(layouts.len(), 2);
    let columns = plan_balanced_columns(&layouts, &config).unwrap();
    let placement = columns[0]
        .placements
        .iter()
        .find(|placement| placement.block_index == 1)
        .expect("second code block should begin in the first column");
    assert_eq!(placement.source_start, 0);
    assert!(placement.y > 0);
    assert!(placement.source_end < layouts[1].total_height);
}

#[test]
fn table_continuation_repeats_header_and_never_splits_rows() {
    let mut markdown = String::from("| Name | Value |\n| --- | ---: |\n");
    for row in 0..24 {
        markdown.push_str(&format!("| row {row} | {row} |\n"));
    }
    let config = NormalizedConfig::new(&RenderConfig {
        max_height: MIN_CONFIGURED_HEIGHT,
        ..RenderConfig::default()
    });
    let mut renderer = RendererState::new().unwrap();
    let fonts = renderer.resolve_config_fonts(&config, false).unwrap();
    let layouts = layout_blocks(
        &mut renderer.font_system,
        collect_blocks(&markdown),
        &config,
        Palette::for_theme("paper"),
        &fonts,
    )
    .unwrap();
    let table = layouts[0].table.as_ref().unwrap();
    let columns = plan_balanced_columns(&layouts, &config).unwrap();
    assert!(columns.len() > 1);
    for column in columns.iter().skip(1) {
        let header = column.placements.first().expect("repeated table header");
        assert_eq!(header.source_start, 0);
        assert_eq!(header.source_end, table.header_height);
    }
    for placement in columns.iter().flat_map(|column| &column.placements) {
        assert!(
            placement.source_start == 0 || layouts[0].boundaries.contains(&placement.source_start)
        );
        assert!(layouts[0].boundaries.contains(&placement.source_end));
    }
}

#[test]
fn table_columns_follow_content_instead_of_splitting_ids() {
    // 赞助榜的形状:两列只装一个字符的序号,一列九到十位的 QQ 号。等分时每列
    // 正文宽只有 960/5 - 28 = 164 px,十位号码约 190 px,cosmic-text 找不到可断
    // 的词就回落到逐字切,于是 "3058704216" 被劈成两行——而序号列那 164 px
    // 大半是空的。
    let markdown = concat!(
        "| 排名 | 赞助人 | QQ | 金额 | 笔数 |\n",
        "| --- | --- | ---: | ---: | ---: |\n",
        "| 1 | 沐风 | 596113920 | ¥49.00 | 1 |\n",
        "| 2 | RyanZ | 3058704216 | ¥30.00 | 3 |\n",
    );
    let config = NormalizedConfig::new(&RenderConfig::default());
    let mut renderer = RendererState::new().unwrap();
    let fonts = renderer.resolve_config_fonts(&config, false).unwrap();
    let layouts = layout_blocks(
        &mut renderer.font_system,
        collect_blocks(markdown),
        &config,
        Palette::for_theme("paper"),
        &fonts,
    )
    .unwrap();
    let table = layouts[0].table.as_ref().unwrap();
    let widths = table.rows[0]
        .cells
        .iter()
        .map(|cell| cell.width)
        .collect::<Vec<_>>();
    assert_eq!(
        widths.iter().sum::<u32>(),
        COLUMN_WIDTH,
        "列宽之和必须正好是表格宽度,画最右那根竖线就是靠它: {widths:?}"
    );
    assert!(
        widths[1] > widths[0] && widths[2] > widths[0],
        "赞助人列和 QQ 列应当比序号列宽: {widths:?}"
    );
    assert!(
        widths[4] < widths[3],
        "只装一位数的笔数列不该比金额列还宽: {widths:?}"
    );
    for row in table.rows.iter().skip(1) {
        assert_eq!(
            row.cells[2].buffer.layout_runs().count(),
            1,
            "QQ 号必须整串排在一行,列宽 {:?}",
            widths
        );
    }
}

#[test]
fn rendered_table_has_grid_header_and_zebra_backgrounds() {
    let markdown = "| A | B |\n| --- | --- |\n| one | two |\n| three | four |\n";
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
    let table = layouts[0].table.as_ref().unwrap();
    let header = &table.rows[0];
    let first = &table.rows[1];
    let second = &table.rows[2];
    let page = render(markdown, &raw_config).unwrap().remove(0);
    let image = image::load_from_memory(&page.png).unwrap().to_rgba8();
    let x = config.padding + COLUMN_WIDTH - 5;
    assert_eq!(
        *image.get_pixel(x, config.padding + 5),
        Rgba(palette.table_header_background)
    );
    assert_eq!(
        *image.get_pixel(x, config.padding + first.source_start + 5),
        Rgba(palette.table_background)
    );
    assert_eq!(
        *image.get_pixel(x, config.padding + second.source_start + 5),
        Rgba(palette.quote_background)
    );
    let grid_x = config.padding + header.cells[0].width;
    assert_eq!(
        *image.get_pixel(grid_x, config.padding + header.source_end / 2),
        Rgba(palette.border)
    );
}

#[test]
fn long_content_grows_one_image_past_three_columns() {
    let mut markdown = String::from("```text\n");
    for line in 0..70 {
        markdown.push_str(&format!("line {line:02}: rendered column content\n"));
    }
    markdown.push_str("```\n");
    let config = RenderConfig {
        max_height: MIN_CONFIGURED_HEIGHT,
        ..RenderConfig::default()
    };
    let pages = render(&markdown, &config).unwrap();
    assert_eq!(pages.len(), 1);
    let page = &pages[0];
    let old_three_column_width = config.padding * 2 + COLUMN_WIDTH * 3 + COLUMN_GAP * 2;
    assert!(page.width > old_three_column_width);
    assert!((MIN_RENDERED_HEIGHT..=MIN_CONFIGURED_HEIGHT).contains(&page.height));
    // Balancing shares the trailing partial column across all columns, so
    // the finished image no longer stays pinned at the full page height.
    assert!(page.height < NormalizedConfig::new(&config).max_height);
    assert!(u64::from(page.width) * u64::from(page.height) <= MAX_PAGE_PIXELS);
}

fn code_layouts_for_balancing(lines: u32) -> (NormalizedConfig, Vec<LayoutBlock>) {
    let mut markdown = String::from("```text\n");
    for line in 0..lines {
        markdown.push_str(&format!("line {line:02}: rendered column content\n"));
    }
    markdown.push_str("```\n");
    let config = NormalizedConfig::new(&RenderConfig {
        max_height: MIN_CONFIGURED_HEIGHT,
        ..RenderConfig::default()
    });
    let mut renderer = RendererState::new().unwrap();
    let fonts = renderer.resolve_config_fonts(&config, false).unwrap();
    let layouts = layout_blocks(
        &mut renderer.font_system,
        collect_blocks(&markdown),
        &config,
        Palette::for_theme("paper"),
        &fonts,
    )
    .unwrap();
    (config, layouts)
}

#[test]
fn balanced_columns_have_similar_used_heights() {
    let (config, layouts) = code_layouts_for_balancing(70);
    let usable_height = config.max_height - config.padding * 2;
    let greedy = plan_columns(&layouts, &config).unwrap();
    let balanced = plan_balanced_columns(&layouts, &config).unwrap();
    assert!(balanced.len() > 1);
    let heights = |columns: &[ColumnPlan]| {
        let min = columns.iter().map(|c| c.used_height).min().unwrap();
        let max = columns.iter().map(|c| c.used_height).max().unwrap();
        (min, max)
    };
    let (greedy_min, greedy_max) = heights(&greedy);
    let (balanced_min, balanced_max) = heights(&balanced);
    assert!(balanced_max - balanced_min < usable_height * 30 / 100);
    assert!(balanced_max - balanced_min < greedy_max - greedy_min);
}

#[test]
fn balancing_removes_trailing_sliver_column_and_shrinks_height() {
    let (config, layouts) = code_layouts_for_balancing(60);
    let usable_height = config.max_height - config.padding * 2;
    let greedy = plan_columns(&layouts, &config).unwrap();
    let sliver = greedy.last().unwrap().used_height;
    assert!(
        sliver < usable_height / 4,
        "test premise: greedy leaves a nearly empty last column, got {sliver}"
    );
    let balanced = plan_balanced_columns(&layouts, &config).unwrap();
    assert!(balanced.len() > 1);
    let min = balanced.iter().map(|c| c.used_height).min().unwrap();
    let max = balanced.iter().map(|c| c.used_height).max().unwrap();
    assert!(min * 2 >= max, "no column holds under half of the tallest");
    assert!(
        max + config.padding * 2 < config.max_height,
        "balanced image should shrink below the full page height"
    );
}
