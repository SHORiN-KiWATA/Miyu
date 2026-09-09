//! 块结构 → 带坐标的排版。
//!
//! 分栏是为了控制成图的长宽比（`TARGET_ASPECT_RATIO`）：一张又窄又长的图在聊天
//! 里没法看。`plan_balanced_columns` 试几种栏数，挑最接近目标比例的
//! （`ASPECT_TIE_EPSILON` 用来在几乎相等时偏向更少的栏）。

use crate::platforms::plugins::renderer::*;

pub(in crate::platforms::plugins::renderer) const COLUMN_WIDTH: u32 = 960;

pub(in crate::platforms::plugins::renderer) const COLUMN_GAP: u32 = 32;

pub(in crate::platforms::plugins::renderer) const TARGET_ASPECT_RATIO: f32 = 4.0 / 3.0;

pub(in crate::platforms::plugins::renderer) const ASPECT_TIE_EPSILON: f32 = 0.01;

pub(in crate::platforms::plugins::renderer) const TABLE_CELL_PADDING: u32 = 14;

pub(in crate::platforms::plugins::renderer) struct LayoutBlock {
    pub(in crate::platforms::plugins::renderer) kind: BlockKind,
    pub(in crate::platforms::plugins::renderer) buffer: Option<Buffer>,
    pub(in crate::platforms::plugins::renderer) table: Option<LayoutTable>,
    pub(in crate::platforms::plugins::renderer) task: Option<TaskBox>,
    pub(in crate::platforms::plugins::renderer) total_height: u32,
    pub(in crate::platforms::plugins::renderer) vertical_padding: u32,
    pub(in crate::platforms::plugins::renderer) inset_left: u32,
    pub(in crate::platforms::plugins::renderer) boundaries: Vec<u32>,
    pub(in crate::platforms::plugins::renderer) margin_before: u32,
    pub(in crate::platforms::plugins::renderer) margin_after: u32,
    pub(in crate::platforms::plugins::renderer) default_color: Color,
    pub(in crate::platforms::plugins::renderer) inline_code_background: [u8; 4],
}

pub(in crate::platforms::plugins::renderer) struct LayoutTable {
    pub(in crate::platforms::plugins::renderer) rows: Vec<LayoutTableRow>,
    pub(in crate::platforms::plugins::renderer) header_height: u32,
}

pub(in crate::platforms::plugins::renderer) struct LayoutTableRow {
    pub(in crate::platforms::plugins::renderer) cells: Vec<LayoutTableCell>,
    pub(in crate::platforms::plugins::renderer) source_start: u32,
    pub(in crate::platforms::plugins::renderer) source_end: u32,
    pub(in crate::platforms::plugins::renderer) header: bool,
    pub(in crate::platforms::plugins::renderer) stripe: bool,
}

pub(in crate::platforms::plugins::renderer) struct LayoutTableCell {
    pub(in crate::platforms::plugins::renderer) buffer: Buffer,
    pub(in crate::platforms::plugins::renderer) x: u32,
    pub(in crate::platforms::plugins::renderer) width: u32,
    pub(in crate::platforms::plugins::renderer) default_color: Color,
    pub(in crate::platforms::plugins::renderer) inline_code_background: [u8; 4],
}

#[derive(Clone, Copy)]
pub(in crate::platforms::plugins::renderer) struct TaskBox {
    pub(in crate::platforms::plugins::renderer) checked: bool,
    pub(in crate::platforms::plugins::renderer) x: u32,
    pub(in crate::platforms::plugins::renderer) y: u32,
    pub(in crate::platforms::plugins::renderer) size: u32,
}

pub(in crate::platforms::plugins::renderer) fn layout_blocks(
    font_system: &mut FontSystem,
    blocks: Vec<Block>,
    config: &NormalizedConfig,
    palette: Palette,
    fonts: &ResolvedFonts,
) -> Result<Vec<LayoutBlock>> {
    blocks
        .into_iter()
        .map(|block| layout_block(font_system, block, config, palette, fonts))
        .collect()
}

pub(in crate::platforms::plugins::renderer) fn layout_block(
    font_system: &mut FontSystem,
    block: Block,
    config: &NormalizedConfig,
    palette: Palette,
    fonts: &ResolvedFonts,
) -> Result<LayoutBlock> {
    if block.kind == BlockKind::Rule {
        return Ok(LayoutBlock {
            kind: block.kind,
            buffer: None,
            table: None,
            task: None,
            total_height: 28,
            vertical_padding: 0,
            inset_left: 0,
            boundaries: vec![28],
            margin_before: 20,
            margin_after: 20,
            default_color: color(palette.text),
            inline_code_background: palette.code_background,
        });
    }

    if block.kind == BlockKind::Table {
        return layout_table(
            font_system,
            block
                .table
                .ok_or_else(|| anyhow!("Markdown table is missing its structured rows"))?,
            config,
            palette,
            fonts,
        );
    }

    let (mut inset_left, inset_right, vertical_padding) = block_insets(block.kind);
    let task = block.task.map(|checked| {
        let size = (config.font_size * 3 / 5).clamp(18, 30);
        let marker_x = inset_left.saturating_add(4);
        let marker_y = vertical_padding.saturating_add(
            ((metrics_for(block.kind, InlineStyle::default(), config).line_height as u32)
                .saturating_sub(size))
                / 2,
        );
        inset_left = inset_left.saturating_add(size).saturating_add(16);
        TaskBox {
            checked,
            x: marker_x,
            y: marker_y,
            size,
        }
    });
    let content_width = COLUMN_WIDTH
        .saturating_sub(inset_left)
        .saturating_sub(inset_right)
        .max(64);
    let metrics = metrics_for(block.kind, InlineStyle::default(), config);
    let default_attrs = attrs_for(
        block.kind,
        InlineStyle::default(),
        false,
        metrics,
        palette,
        fonts,
    );
    let expanded = expand_spans(&block.spans, fonts.emoji.is_some());
    let rich_spans = expanded
        .iter()
        .map(|span| {
            let metrics = metrics_for(block.kind, span.style, config);
            let attrs = attrs_for(block.kind, span.style, span.emoji, metrics, palette, fonts);
            (span.text.clone(), attrs)
        })
        .collect::<Vec<_>>();

    let mut buffer = Buffer::new(font_system, metrics);
    buffer.set_size(Some(content_width as f32), None);
    buffer.set_wrap(Wrap::WordOrGlyph);
    buffer.set_rich_text(
        rich_spans
            .iter()
            .map(|(text, attrs)| (text.as_str(), attrs.clone())),
        &default_attrs,
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, true);

    let mut boundaries = Vec::new();
    let mut text_height = 1_u32;
    for run in buffer.layout_runs() {
        let bottom = (run.line_top + run.line_height).ceil().max(1.0) as u32;
        text_height = text_height.max(bottom);
        let boundary = vertical_padding.saturating_add(bottom);
        if boundaries.last().copied() != Some(boundary) {
            boundaries.push(boundary);
        }
    }
    let total_height = text_height.saturating_add(vertical_padding.saturating_mul(2));
    if let Some(last) = boundaries.last_mut() {
        *last = total_height;
    } else {
        boundaries.push(total_height);
    }
    let (margin_before, margin_after) = block_margins(block.kind, config.font_size);
    let default_color = if block.kind == BlockKind::Code {
        palette.code_text
    } else {
        palette.text
    };
    Ok(LayoutBlock {
        kind: block.kind,
        buffer: Some(buffer),
        table: None,
        task,
        total_height,
        vertical_padding,
        inset_left,
        boundaries,
        margin_before,
        margin_after,
        default_color: color(default_color),
        inline_code_background: palette.code_background,
    })
}

pub(in crate::platforms::plugins::renderer) fn layout_table(
    font_system: &mut FontSystem,
    table: TableBlock,
    config: &NormalizedConfig,
    palette: Palette,
    fonts: &ResolvedFonts,
) -> Result<LayoutBlock> {
    let column_count = table
        .alignments
        .len()
        .max(table.header.len())
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    if column_count == 0 {
        bail!("Markdown table has no columns");
    }
    let column_count_u32 =
        u32::try_from(column_count).context("too many Markdown table columns")?;
    if COLUMN_WIDTH / column_count_u32 <= TABLE_CELL_PADDING.saturating_mul(2) {
        bail!("Markdown table has too many columns to render safely");
    }

    // 先量一遍每列的自然宽度（不换行时要多宽），再按内容分配。等分会让只装
    // 一个字符的序号列和九位 QQ 号列一样宽，后者装不下就被逐字劈成两行。
    let mut natural = vec![0_u32; column_count];
    measure_row(
        font_system,
        &table.header,
        true,
        config,
        palette,
        fonts,
        &mut natural,
    );
    for cells in &table.rows {
        measure_row(
            font_system,
            cells,
            false,
            config,
            palette,
            fonts,
            &mut natural,
        );
    }
    let metrics = metrics_for(BlockKind::Table, InlineStyle::default(), config);
    let min_content = (metrics.font_size * 2.0).ceil().max(1.0) as u32;
    let widths = plan_table_columns(&natural, COLUMN_WIDTH, TABLE_CELL_PADDING, min_content);

    let mut rows = Vec::with_capacity(table.rows.len().saturating_add(1));
    let mut source_y = 0_u32;
    if !table.header.is_empty() {
        let row = layout_table_row(
            font_system,
            &table.header,
            &table.alignments,
            &widths,
            true,
            false,
            source_y,
            config,
            palette,
            fonts,
        )?;
        source_y = row.source_end;
        rows.push(row);
    }
    let header_height = source_y;
    for (index, cells) in table.rows.iter().enumerate() {
        let row = layout_table_row(
            font_system,
            cells,
            &table.alignments,
            &widths,
            false,
            index % 2 == 1,
            source_y,
            config,
            palette,
            fonts,
        )?;
        source_y = row.source_end;
        rows.push(row);
    }
    let boundaries = rows.iter().map(|row| row.source_end).collect::<Vec<_>>();
    let (margin_before, margin_after) = block_margins(BlockKind::Table, config.font_size);
    Ok(LayoutBlock {
        kind: BlockKind::Table,
        buffer: None,
        table: Some(LayoutTable {
            rows,
            header_height,
        }),
        task: None,
        total_height: source_y,
        vertical_padding: 0,
        inset_left: 0,
        boundaries,
        margin_before,
        margin_after,
        default_color: color(palette.text),
        inline_code_background: palette.code_background,
    })
}

/// 把一行单元格的自然宽度并进 `natural`（逐列取最大值）。
#[allow(clippy::too_many_arguments)]
fn measure_row(
    font_system: &mut FontSystem,
    cells: &[Vec<RichSpan>],
    header: bool,
    config: &NormalizedConfig,
    palette: Palette,
    fonts: &ResolvedFonts,
    natural: &mut [u32],
) {
    for (index, spans) in cells.iter().enumerate() {
        let Some(slot) = natural.get_mut(index) else {
            break;
        };
        let width = natural_text_width(
            font_system,
            spans,
            BlockKind::Table,
            header,
            config,
            palette,
            fonts,
        );
        *slot = (*slot).max(width);
    }
}

/// 不设宽度约束、关掉换行地塑形一次，读回实际排出来的行宽。
#[allow(clippy::too_many_arguments)]
fn natural_text_width(
    font_system: &mut FontSystem,
    spans: &[RichSpan],
    kind: BlockKind,
    force_bold: bool,
    config: &NormalizedConfig,
    palette: Palette,
    fonts: &ResolvedFonts,
) -> u32 {
    if spans.is_empty() {
        return 0;
    }
    let buffer = shape_rich_buffer(
        font_system,
        spans,
        kind,
        None,
        Wrap::None,
        force_bold,
        Alignment::None,
        config,
        palette,
        fonts,
    );
    buffer
        .layout_runs()
        .map(|run| run.line_w.ceil().max(0.0) as u32)
        .max()
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::platforms::plugins::renderer) fn layout_table_row(
    font_system: &mut FontSystem,
    cells: &[Vec<RichSpan>],
    alignments: &[Alignment],
    widths: &[u32],
    header: bool,
    stripe: bool,
    source_start: u32,
    config: &NormalizedConfig,
    palette: Palette,
    fonts: &ResolvedFonts,
) -> Result<LayoutTableRow> {
    let metrics = metrics_for(BlockKind::Table, InlineStyle::default(), config);
    let mut x = 0_u32;
    let mut row_height = metrics.line_height.ceil().max(1.0) as u32;
    let mut laid_out = Vec::with_capacity(widths.len());
    for (index, width) in widths.iter().copied().enumerate() {
        let content_width = width.saturating_sub(TABLE_CELL_PADDING.saturating_mul(2));
        let spans = cells.get(index).map(Vec::as_slice).unwrap_or(&[]);
        let alignment = alignments.get(index).copied().unwrap_or(Alignment::None);
        let (buffer, text_height, default_color) = layout_rich_buffer(
            font_system,
            spans,
            BlockKind::Table,
            content_width,
            header,
            alignment,
            config,
            palette,
            fonts,
        );
        row_height = row_height.max(text_height);
        laid_out.push(LayoutTableCell {
            buffer,
            x,
            width,
            default_color,
            inline_code_background: palette.code_background,
        });
        x = x
            .checked_add(width)
            .context("Markdown table width overflowed")?;
    }
    row_height = row_height.saturating_add(TABLE_CELL_PADDING.saturating_mul(2));
    let source_end = source_start
        .checked_add(row_height)
        .context("Markdown table height overflowed")?;
    Ok(LayoutTableRow {
        cells: laid_out,
        source_start,
        source_end,
        header,
        stripe,
    })
}

#[allow(clippy::too_many_arguments)]
pub(in crate::platforms::plugins::renderer) fn layout_rich_buffer(
    font_system: &mut FontSystem,
    spans: &[RichSpan],
    kind: BlockKind,
    width: u32,
    force_bold: bool,
    alignment: Alignment,
    config: &NormalizedConfig,
    palette: Palette,
    fonts: &ResolvedFonts,
) -> (Buffer, u32, Color) {
    let metrics = metrics_for(kind, InlineStyle::default(), config);
    let buffer = shape_rich_buffer(
        font_system,
        spans,
        kind,
        Some(width),
        Wrap::WordOrGlyph,
        force_bold,
        alignment,
        config,
        palette,
        fonts,
    );
    let text_height = buffer
        .layout_runs()
        .map(|run| (run.line_top + run.line_height).ceil().max(1.0) as u32)
        .max()
        .unwrap_or_else(|| metrics.line_height.ceil().max(1.0) as u32);
    (buffer, text_height, color(palette.text))
}

/// 塑形一段富文本。`width` 为 `None` 时不设宽度约束——量自然宽度用。
#[allow(clippy::too_many_arguments)]
fn shape_rich_buffer(
    font_system: &mut FontSystem,
    spans: &[RichSpan],
    kind: BlockKind,
    width: Option<u32>,
    wrap: Wrap,
    force_bold: bool,
    alignment: Alignment,
    config: &NormalizedConfig,
    palette: Palette,
    fonts: &ResolvedFonts,
) -> Buffer {
    let metrics = metrics_for(kind, InlineStyle::default(), config);
    let default_attrs = attrs_for(
        kind,
        InlineStyle {
            bold: force_bold,
            ..InlineStyle::default()
        },
        false,
        metrics,
        palette,
        fonts,
    );
    let mut expanded = expand_spans(spans, fonts.emoji.is_some());
    if expanded.is_empty() {
        expanded.push(ExpandedSpan {
            text: " ".to_string(),
            style: InlineStyle::default(),
            emoji: false,
        });
    }
    let rich_spans = expanded
        .iter()
        .map(|span| {
            let mut style = span.style;
            style.bold |= force_bold;
            let metrics = metrics_for(kind, style, config);
            let attrs = attrs_for(kind, style, span.emoji, metrics, palette, fonts);
            (span.text.clone(), attrs)
        })
        .collect::<Vec<_>>();
    let alignment = match alignment {
        Alignment::Right => Some(TextAlign::Right),
        Alignment::Center => Some(TextAlign::Center),
        Alignment::Left => Some(TextAlign::Left),
        Alignment::None => None,
    };
    let mut buffer = Buffer::new(font_system, metrics);
    buffer.set_size(width.map(|width| width.max(1) as f32), None);
    buffer.set_wrap(wrap);
    buffer.set_rich_text(
        rich_spans
            .iter()
            .map(|(text, attrs)| (text.as_str(), attrs.clone())),
        &default_attrs,
        Shaping::Advanced,
        alignment,
    );
    buffer.shape_until_scroll(font_system, true);
    buffer
}

#[derive(Clone)]
pub(in crate::platforms::plugins::renderer) struct ExpandedSpan {
    pub(in crate::platforms::plugins::renderer) text: String,
    pub(in crate::platforms::plugins::renderer) style: InlineStyle,
    pub(in crate::platforms::plugins::renderer) emoji: bool,
}

pub(in crate::platforms::plugins::renderer) fn expand_spans(
    spans: &[RichSpan],
    split_emoji: bool,
) -> Vec<ExpandedSpan> {
    let mut expanded: Vec<ExpandedSpan> = Vec::new();
    for span in spans {
        if !split_emoji {
            expanded.push(ExpandedSpan {
                text: span.text.clone(),
                style: span.style,
                emoji: false,
            });
            continue;
        }
        for grapheme in span.text.graphemes(true) {
            let emoji = grapheme_is_emoji(grapheme);
            if let Some(last) = expanded
                .last_mut()
                .filter(|last| last.style == span.style && last.emoji == emoji)
            {
                last.text.push_str(grapheme);
            } else {
                expanded.push(ExpandedSpan {
                    text: grapheme.to_string(),
                    style: span.style,
                    emoji,
                });
            }
        }
    }
    expanded
}

pub(in crate::platforms::plugins::renderer) fn markdown_contains_emoji(markdown: &str) -> bool {
    markdown.graphemes(true).any(grapheme_is_emoji)
}

pub(in crate::platforms::plugins::renderer) fn grapheme_is_emoji(grapheme: &str) -> bool {
    grapheme.chars().any(|ch| {
        matches!(
            ch as u32,
            0x1F000..=0x1FAFF
                | 0x2300..=0x23FF
                | 0x2600..=0x27BF
                | 0x2B00..=0x2BFF
                | 0xFE0F
                | 0x200D
        )
    })
}

pub(in crate::platforms::plugins::renderer) fn attrs_for<'a>(
    kind: BlockKind,
    style: InlineStyle,
    emoji: bool,
    metrics: Metrics,
    palette: Palette,
    fonts: &'a ResolvedFonts,
) -> Attrs<'a> {
    let named = if emoji {
        fonts.emoji.as_deref()
    } else if style.code || matches!(kind, BlockKind::Code) {
        fonts.code.as_deref()
    } else if matches!(kind, BlockKind::Heading(_)) {
        fonts.title.as_deref().or(fonts.body.as_deref())
    } else {
        fonts.body.as_deref()
    };
    let fallback = if style.code || matches!(kind, BlockKind::Code) {
        Family::Monospace
    } else {
        Family::SansSerif
    };
    let family = named.map(Family::Name).unwrap_or(fallback);
    let foreground = if matches!(kind, BlockKind::Code) {
        palette.code_text
    } else if style.code {
        palette.code_text
    } else if style.link {
        palette.link
    } else if style.muted {
        palette.muted
    } else if matches!(kind, BlockKind::Heading(_)) {
        palette.heading
    } else if style.bold {
        // 只有 Regular 字重可用,Weight::BOLD 是空请求——加粗靠变色呈现。
        palette.strong
    } else {
        palette.text
    };
    let mut attrs = Attrs::new()
        .family(family)
        .color(color(foreground))
        .metrics(metrics);
    if style.bold || matches!(kind, BlockKind::Heading(_)) {
        attrs = attrs.weight(Weight::BOLD);
    }
    if style.italic {
        attrs = attrs.style(FontStyle::Italic);
    }
    if style.code && !matches!(kind, BlockKind::Code) {
        // 行内代码经 metadata 传到 LayoutGlyph,绘制时据此画底色小块;
        // 代码块整块已有背景,不标。
        attrs = attrs.metadata(INLINE_CODE_METADATA);
    }
    attrs
}

pub(in crate::platforms::plugins::renderer) const INLINE_CODE_METADATA: usize = 1;

/// 一条 layout 行内行内代码字形的连续 x 区间(相邻区间合并)。
pub(in crate::platforms::plugins::renderer) fn inline_code_chip_ranges(
    glyphs: &[LayoutGlyph],
) -> Vec<(f32, f32)> {
    let mut ranges: Vec<(f32, f32)> = Vec::new();
    for glyph in glyphs {
        if glyph.metadata != INLINE_CODE_METADATA {
            continue;
        }
        let start = glyph.x;
        let end = glyph.x + glyph.w;
        match ranges.last_mut() {
            Some((_, last_end)) if start - *last_end <= 0.5 => *last_end = end.max(*last_end),
            _ => ranges.push((start, end)),
        }
    }
    ranges
}

/// 行内代码底色块的水平/垂直留白。
pub(in crate::platforms::plugins::renderer) const INLINE_CODE_CHIP_PAD_X: f32 = 5.0;

pub(in crate::platforms::plugins::renderer) const INLINE_CODE_CHIP_INSET_RATIO: f32 = 0.10;

pub(in crate::platforms::plugins::renderer) fn metrics_for(
    kind: BlockKind,
    style: InlineStyle,
    config: &NormalizedConfig,
) -> Metrics {
    let body = config.font_size as f32;
    let code = config.code_font_size as f32;
    let size = match kind {
        BlockKind::Heading(level) => {
            let scale = match level {
                1 => 1.55,
                2 => 1.35,
                3 => 1.20,
                4 => 1.10,
                _ => 1.0,
            };
            (body * scale).min(76.0)
        }
        BlockKind::Code => code,
        BlockKind::Table => (body * 0.92).max(14.0),
        _ if style.code => code,
        _ => body,
    };
    Metrics::new(size, (size * 1.42).ceil())
}

pub(in crate::platforms::plugins::renderer) fn block_insets(kind: BlockKind) -> (u32, u32, u32) {
    match kind {
        BlockKind::Code => (32, 32, 24),
        BlockKind::Table => (20, 20, 16),
        BlockKind::Quote => (32, 14, 12),
        BlockKind::ListItem { depth } => {
            (u32::from(depth.saturating_sub(1)).saturating_mul(18), 0, 0)
        }
        _ => (0, 0, 0),
    }
}

pub(in crate::platforms::plugins::renderer) fn block_margins(
    kind: BlockKind,
    font_size: u32,
) -> (u32, u32) {
    let small = (font_size / 4).max(6);
    match kind {
        BlockKind::Heading(1) => (font_size, font_size / 2),
        BlockKind::Heading(_) => (font_size / 2, small),
        BlockKind::Code | BlockKind::Table => (font_size / 2, font_size / 2),
        BlockKind::Rule => (font_size / 2, font_size / 2),
        BlockKind::Quote => (small, small),
        BlockKind::ListItem { .. } => (small / 2, small / 2),
        BlockKind::Paragraph => (small, small),
    }
}

#[derive(Default)]
pub(in crate::platforms::plugins::renderer) struct ColumnPlan {
    pub(in crate::platforms::plugins::renderer) placements: Vec<Placement>,
    pub(in crate::platforms::plugins::renderer) used_height: u32,
}

pub(in crate::platforms::plugins::renderer) struct Placement {
    pub(in crate::platforms::plugins::renderer) block_index: usize,
    pub(in crate::platforms::plugins::renderer) source_start: u32,
    pub(in crate::platforms::plugins::renderer) source_end: u32,
    pub(in crate::platforms::plugins::renderer) y: u32,
}

pub(in crate::platforms::plugins::renderer) fn plan_columns(
    layouts: &[LayoutBlock],
    config: &NormalizedConfig,
) -> Result<Vec<ColumnPlan>> {
    let usable_height = config
        .max_height
        .saturating_sub(config.padding.saturating_mul(2));
    plan_columns_with_height(layouts, usable_height)
}

pub(in crate::platforms::plugins::renderer) fn plan_columns_with_height(
    layouts: &[LayoutBlock],
    usable_height: u32,
) -> Result<Vec<ColumnPlan>> {
    if usable_height < 128 {
        bail!("page height leaves too little room for rendered content");
    }
    let mut columns = vec![ColumnPlan::default()];

    for (block_index, block) in layouts.iter().enumerate() {
        if let Some(table) = block.table.as_ref() {
            if table.header_height > usable_height {
                bail!("a Markdown table header exceeds the usable image height");
            }
            for row in table.rows.iter().filter(|row| !row.header) {
                let row_height = row.source_end.saturating_sub(row.source_start);
                if table.header_height.saturating_add(row_height) > usable_height {
                    bail!("a Markdown table row exceeds the usable image height");
                }
            }
        }
        let mut source_start = 0;
        let mut first_fragment = true;
        while source_start < block.total_height {
            if source_start > 0 {
                if let Some(table) = block
                    .table
                    .as_ref()
                    .filter(|table| table.header_height > 0 && source_start >= table.header_height)
                {
                    let column = columns
                        .last_mut()
                        .ok_or_else(|| anyhow!("renderer column planner lost its active column"))?;
                    if column.used_height == 0 {
                        column.placements.push(Placement {
                            block_index,
                            source_start: 0,
                            source_end: table.header_height,
                            y: 0,
                        });
                        column.used_height = table.header_height;
                    }
                }
            }
            let column = columns
                .last_mut()
                .ok_or_else(|| anyhow!("renderer column planner lost its active column"))?;
            let margin = if first_fragment && column.used_height > 0 {
                block.margin_before
            } else {
                0
            };
            let remaining = block.total_height.saturating_sub(source_start);
            let available = usable_height
                .saturating_sub(column.used_height)
                .saturating_sub(margin);

            if first_fragment && column.used_height > 0 {
                if let Some(table) = block.table.as_ref() {
                    let first_body_height = table
                        .rows
                        .iter()
                        .find(|row| !row.header)
                        .map(|row| row.source_end.saturating_sub(row.source_start))
                        .unwrap_or(0);
                    let first_table_chunk = table.header_height.saturating_add(first_body_height);
                    if first_table_chunk > available && first_table_chunk <= usable_height {
                        push_column(&mut columns)?;
                        continue;
                    }
                }
            }

            if first_fragment
                && block.kind != BlockKind::Code
                && block.total_height <= usable_height
                && remaining > available
                && column.used_height > 0
            {
                push_column(&mut columns)?;
                continue;
            }
            if available == 0 {
                push_column(&mut columns)?;
                continue;
            }

            let limit = source_start.saturating_add(available);
            let source_end = if remaining <= available {
                block.total_height
            } else {
                block
                    .boundaries
                    .iter()
                    .copied()
                    .take_while(|boundary| *boundary <= limit)
                    .last()
                    .unwrap_or(source_start)
            };
            if source_end <= source_start {
                if column.used_height == 0 {
                    bail!("a rendered text line exceeds the usable page height");
                }
                push_column(&mut columns)?;
                continue;
            }

            let y = column.used_height.saturating_add(margin);
            column.placements.push(Placement {
                block_index,
                source_start,
                source_end,
                y,
            });
            column.used_height = y.saturating_add(source_end.saturating_sub(source_start));
            source_start = source_end;
            first_fragment = false;
            if source_start < block.total_height {
                push_column(&mut columns)?;
            } else {
                column.used_height = column
                    .used_height
                    .saturating_add(block.margin_after)
                    .min(usable_height);
            }
        }
    }
    Ok(columns)
}

pub(in crate::platforms::plugins::renderer) fn push_column(
    columns: &mut Vec<ColumnPlan>,
) -> Result<()> {
    columns
        .len()
        .checked_add(1)
        .context("rendered Markdown column count overflowed")?;
    columns.push(ColumnPlan::default());
    Ok(())
}

/// Plans columns and then rebalances them so multi-column images approach the
/// target aspect ratio instead of leaving a nearly empty trailing column.
///
/// The full-height greedy plan fixes the column-count ceiling `n_max` (and
/// propagates any planning error unchanged). For every candidate column count
/// a binary search finds the smallest usable column height that still fits in
/// that many columns; planner errors or overflowing column counts during the
/// search are treated as "too short" rather than fatal. The candidate whose
/// overall image is closest to `TARGET_ASPECT_RATIO` (log-distance, ties going
/// to fewer columns) wins.
pub(in crate::platforms::plugins::renderer) fn plan_balanced_columns(
    layouts: &[LayoutBlock],
    config: &NormalizedConfig,
) -> Result<Vec<ColumnPlan>> {
    let max_usable = config
        .max_height
        .saturating_sub(config.padding.saturating_mul(2));
    let full_plan = plan_columns_with_height(layouts, max_usable)?;
    let column_ceiling = full_plan.len();
    if column_ceiling <= 1 {
        return Ok(full_plan);
    }

    let total_content: u64 = layouts
        .iter()
        .map(|block| u64::from(block.total_height))
        .sum();
    let height_floor = u64::from(
        MIN_RENDERED_HEIGHT
            .saturating_sub(config.padding.saturating_mul(2))
            .max(128),
    );
    let mut best: Option<(Vec<ColumnPlan>, f32)> = None;
    for candidate in 1..=column_ceiling {
        let low = total_content
            .div_ceil(candidate as u64)
            .max(height_floor)
            .min(u64::from(max_usable)) as u32;
        let Some(plan) = balanced_plan_for_count(layouts, candidate, low, max_usable) else {
            continue;
        };
        let distance = aspect_distance(&plan, config);
        let improves = best
            .as_ref()
            .map(|(_, best_distance)| distance + ASPECT_TIE_EPSILON < *best_distance)
            .unwrap_or(true);
        if improves {
            best = Some((plan, distance));
        }
    }
    Ok(best.map(|(plan, _)| plan).unwrap_or(full_plan))
}

/// Binary-searches the smallest usable height in `[low, high]` whose plan fits
/// in at most `target_columns` columns. Returns `None` when even the full
/// height `high` cannot satisfy the target.
pub(in crate::platforms::plugins::renderer) fn balanced_plan_for_count(
    layouts: &[LayoutBlock],
    target_columns: usize,
    low: u32,
    high: u32,
) -> Option<Vec<ColumnPlan>> {
    let mut best = match plan_columns_with_height(layouts, high) {
        Ok(plan) if plan.len() <= target_columns => plan,
        _ => return None,
    };
    let mut low = low.min(high);
    let mut high = high;
    while low < high {
        let mid = low + (high - low) / 2;
        match plan_columns_with_height(layouts, mid) {
            Ok(plan) if plan.len() <= target_columns => {
                best = plan;
                high = mid;
            }
            _ => low = mid.saturating_add(1),
        }
    }
    Some(best)
}

/// Log-space distance between the finished image's aspect ratio (using the
/// same width/height rules as `render_pages`) and `TARGET_ASPECT_RATIO`.
pub(in crate::platforms::plugins::renderer) fn aspect_distance(
    columns: &[ColumnPlan],
    config: &NormalizedConfig,
) -> f32 {
    let count = columns.len() as u64;
    let width = u64::from(config.padding) * 2
        + u64::from(COLUMN_WIDTH) * count
        + u64::from(COLUMN_GAP) * count.saturating_sub(1);
    let content_height = columns
        .iter()
        .map(|column| column.used_height)
        .max()
        .unwrap_or(0);
    let height = content_height
        .saturating_add(config.padding.saturating_mul(2))
        .clamp(MIN_RENDERED_HEIGHT, config.max_height);
    ((width as f32 / height as f32).ln() - TARGET_ASPECT_RATIO.ln()).abs()
}
