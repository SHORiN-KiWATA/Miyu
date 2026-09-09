//! 排版 → 像素。
//!
//! 每一层都有上限：单页像素、总像素、单页 PNG 字节、总字节
//! （`MAX_*_PIXELS` / `MAX_*_PNG_BYTES`）。`CappedVecWriter` 让 PNG 编码在超限时
//! 当场失败，而不是先编出一个几百兆的缓冲再检查。

use crate::platforms::plugins::renderer::*;

pub(in crate::platforms::plugins::renderer) const MAX_PAGE_PIXELS: u64 = 20_000_000;

pub(in crate::platforms::plugins::renderer) const MAX_TOTAL_PAGE_PIXELS: u64 = 48_000_000;

pub(in crate::platforms::plugins::renderer) const MAX_PAGE_PNG_BYTES: usize = 20 * 1024 * 1024;

pub(in crate::platforms::plugins::renderer) const MAX_TOTAL_PNG_BYTES: usize = 48 * 1024 * 1024;

pub(in crate::platforms::plugins::renderer) const MIN_CONFIGURED_HEIGHT: u32 = 1000;

pub(in crate::platforms::plugins::renderer) const MIN_RENDERED_HEIGHT: u32 = 360;

pub(in crate::platforms::plugins::renderer) const MAX_PAGE_HEIGHT: u32 = 5000;

#[derive(Clone, Copy)]
pub(in crate::platforms::plugins::renderer) struct Palette {
    pub(in crate::platforms::plugins::renderer) background: [u8; 4],
    pub(in crate::platforms::plugins::renderer) text: [u8; 4],
    pub(in crate::platforms::plugins::renderer) heading: [u8; 4],
    pub(in crate::platforms::plugins::renderer) muted: [u8; 4],
    pub(in crate::platforms::plugins::renderer) link: [u8; 4],
    /// 加粗文本的替代色:资产只带 Regular 字重(CJK 粗体一个 15MB+,低占用
    /// 专项不背),**加粗**以变色呈现(08-25 用户裁定)。
    pub(in crate::platforms::plugins::renderer) strong: [u8; 4],
    pub(in crate::platforms::plugins::renderer) code_background: [u8; 4],
    pub(in crate::platforms::plugins::renderer) code_text: [u8; 4],
    pub(in crate::platforms::plugins::renderer) quote_background: [u8; 4],
    pub(in crate::platforms::plugins::renderer) quote_bar: [u8; 4],
    pub(in crate::platforms::plugins::renderer) table_header_background: [u8; 4],
    pub(in crate::platforms::plugins::renderer) table_background: [u8; 4],
    pub(in crate::platforms::plugins::renderer) border: [u8; 4],
    pub(in crate::platforms::plugins::renderer) rule: [u8; 4],
}

impl Palette {
    pub(in crate::platforms::plugins::renderer) fn for_theme(theme: &str) -> Self {
        match theme {
            "dark" => Self {
                background: [28, 29, 32, 255],
                text: [231, 232, 235, 255],
                heading: [255, 255, 255, 255],
                muted: [164, 168, 176, 255],
                link: [104, 179, 255, 255],
                strong: [229, 192, 123, 255],
                code_background: [43, 45, 51, 255],
                code_text: [239, 240, 244, 255],
                quote_background: [37, 40, 45, 255],
                quote_bar: [93, 168, 143, 255],
                table_header_background: [19, 20, 23, 255],
                table_background: [34, 36, 40, 255],
                border: [72, 76, 84, 255],
                rule: [83, 87, 95, 255],
            },
            "light" => Self {
                background: [250, 250, 248, 255],
                text: [30, 34, 40, 255],
                heading: [18, 20, 24, 255],
                muted: [92, 96, 104, 255],
                link: [48, 101, 190, 255],
                strong: [176, 112, 14, 255],
                code_background: [226, 229, 235, 255],
                code_text: [34, 38, 45, 255],
                quote_background: [244, 247, 255, 255],
                quote_bar: [74, 116, 214, 255],
                table_header_background: [238, 240, 244, 255],
                table_background: [246, 247, 249, 255],
                border: [218, 222, 230, 255],
                rule: [218, 222, 230, 255],
            },
            _ => Self {
                background: [244, 239, 229, 255],
                text: [48, 46, 41, 255],
                heading: [37, 34, 29, 255],
                muted: [104, 98, 88, 255],
                // 深青蓝。原来是 [112, 82, 43] 暖棕,和 strong 的橙棕同色相
                // ——而加粗只能靠变色呈现(字体资产只带 Regular),于是链接和
                // 加粗在图里长得一样。链接要的是「另一类东西」而不是「更强
                // 调」,所以离开暖色系;dark / light 两个主题的链接本来就是
                // 蓝的,这下三个主题同一个语义。
                link: [45, 95, 125, 255],
                strong: [153, 88, 28, 255],
                code_background: [225, 219, 208, 255],
                code_text: [42, 39, 34, 255],
                quote_background: [236, 229, 214, 255],
                quote_bar: [134, 101, 54, 255],
                table_header_background: [232, 226, 215, 255],
                table_background: [239, 233, 222, 255],
                border: [211, 201, 184, 255],
                rule: [211, 201, 184, 255],
            },
        }
    }
}

#[cfg(test)]
mod palette_tests {
    use super::Palette;

    /// 加粗、标题、链接都只能靠颜色区分(字体资产只带 Regular 字重),所以
    /// 「链接色不得撞上强调色」是这套调色板的硬约束,不是审美偏好。
    #[test]
    fn links_never_share_a_colour_with_emphasis() {
        for theme in ["paper", "light", "dark", "unknown-falls-back-to-paper"] {
            let palette = Palette::for_theme(theme);
            assert_ne!(palette.link, palette.strong, "theme {theme}");
            assert_ne!(palette.link, palette.heading, "theme {theme}");
            assert_ne!(palette.link, palette.text, "theme {theme}");
            assert_ne!(palette.link, palette.muted, "theme {theme}");
        }
    }
}

pub(in crate::platforms::plugins::renderer) fn render_pages(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    layouts: &[LayoutBlock],
    columns: &[ColumnPlan],
    config: &NormalizedConfig,
    palette: Palette,
) -> Result<Vec<RenderedImage>> {
    let column_count = u32::try_from(columns.len()).context("too many image columns")?;
    let columns_width = COLUMN_WIDTH
        .checked_mul(column_count)
        .context("rendered image width overflowed")?;
    let gaps_width = COLUMN_GAP
        .checked_mul(column_count.saturating_sub(1))
        .context("rendered image gap width overflowed")?;
    let width = config
        .padding
        .checked_mul(2)
        .and_then(|padding| padding.checked_add(columns_width))
        .and_then(|width| width.checked_add(gaps_width))
        .context("rendered image width overflowed")?;
    let content_height = columns
        .iter()
        .map(|column| column.used_height)
        .max()
        .unwrap_or(0);
    let height = content_height
        .saturating_add(config.padding.saturating_mul(2))
        .clamp(MIN_RENDERED_HEIGHT, config.max_height);
    validate_page_dimensions(width, height)?;
    let pixels = u64::from(width) * u64::from(height);
    checked_total_page_pixels(0, pixels)?;

    let mut image = RgbaImage::from_pixel(width, height, Rgba(palette.background));
    for (column_index, column) in columns.iter().enumerate() {
        let column_index =
            u32::try_from(column_index).context("image column index does not fit in u32")?;
        let column_x = config
            .padding
            .saturating_add(column_index.saturating_mul(COLUMN_WIDTH.saturating_add(COLUMN_GAP)));
        for placement in &column.placements {
            let block = layouts
                .get(placement.block_index)
                .ok_or_else(|| anyhow!("renderer placement references a missing block"))?;
            let destination_y = config.padding.saturating_add(placement.y);
            if block.table.is_some() {
                draw_table_fragment(
                    &mut image,
                    font_system,
                    swash_cache,
                    block,
                    placement,
                    column_x,
                    destination_y,
                    palette,
                );
                continue;
            }
            draw_decoration(
                &mut image,
                block,
                placement,
                column_x,
                destination_y,
                palette,
            );
            draw_text_fragment(
                &mut image,
                font_system,
                swash_cache,
                block,
                placement,
                column_x,
                destination_y,
            );
        }
    }

    let png_limit = MAX_PAGE_PNG_BYTES.min(MAX_TOTAL_PNG_BYTES);
    let mut writer = CappedVecWriter::new(png_limit);
    let encoded = PngEncoder::new(&mut writer).write_image(
        image.as_raw(),
        width,
        height,
        ColorType::Rgba8.into(),
    );
    if let Err(error) = encoded {
        if writer.exceeded() {
            bail!("rendered image exceeds the {png_limit}-byte PNG limit");
        }
        return Err(error).context("failed to encode rendered Markdown as PNG");
    }
    let png = writer.into_inner();
    Ok(vec![RenderedImage {
        mime: "image/png".to_string(),
        png,
        width,
        height,
    }])
}

pub(in crate::platforms::plugins::renderer) struct CappedVecWriter {
    pub(in crate::platforms::plugins::renderer) bytes: Vec<u8>,
    pub(in crate::platforms::plugins::renderer) limit: usize,
    pub(in crate::platforms::plugins::renderer) exceeded: bool,
}

impl CappedVecWriter {
    pub(in crate::platforms::plugins::renderer) fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    pub(in crate::platforms::plugins::renderer) fn exceeded(&self) -> bool {
        self.exceeded
    }

    pub(in crate::platforms::plugins::renderer) fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for CappedVecWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(buffer.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("rendered PNG byte budget exceeded"));
        };
        if next_len > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("rendered PNG byte budget exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(in crate::platforms::plugins::renderer) fn validate_page_dimensions(
    width: u32,
    height: u32,
) -> Result<()> {
    if width == 0 {
        bail!("rendered image width must be non-zero");
    }
    if !(MIN_RENDERED_HEIGHT..=MAX_PAGE_HEIGHT).contains(&height) {
        bail!("rendered image height {height} is outside the supported range");
    }
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if pixels > MAX_PAGE_PIXELS {
        bail!("rendered image would exceed the {MAX_PAGE_PIXELS}-pixel limit");
    }
    Ok(())
}

pub(in crate::platforms::plugins::renderer) fn checked_total_page_pixels(
    current: u64,
    page: u64,
) -> Result<u64> {
    let total = current
        .checked_add(page)
        .context("rendered page pixel count overflowed")?;
    if total > MAX_TOTAL_PAGE_PIXELS {
        bail!("rendered Markdown exceeds the {MAX_TOTAL_PAGE_PIXELS}-pixel total limit");
    }
    Ok(total)
}

pub(in crate::platforms::plugins::renderer) fn draw_decoration(
    image: &mut RgbaImage,
    block: &LayoutBlock,
    placement: &Placement,
    x: u32,
    y: u32,
    palette: Palette,
) {
    let height = placement.source_end.saturating_sub(placement.source_start);
    match block.kind {
        BlockKind::Code => {
            fill_rounded_rect(
                image,
                x,
                y,
                COLUMN_WIDTH,
                height,
                palette.code_background,
                12,
                placement.source_start == 0,
                placement.source_end == block.total_height,
            );
        }
        BlockKind::Quote => {
            fill_rect(image, x, y, COLUMN_WIDTH, height, palette.quote_background);
            fill_rect(image, x, y, 6, height, palette.quote_bar);
        }
        BlockKind::Rule => {
            let line_y = y.saturating_add(height / 2);
            fill_rect(image, x, line_y, COLUMN_WIDTH, 2, palette.rule);
        }
        BlockKind::Heading(1) if placement.source_end == block.total_height => {
            let line_y = y.saturating_add(height).saturating_sub(2);
            fill_rect(image, x, line_y, COLUMN_WIDTH, 2, palette.rule);
        }
        _ => {}
    }
    if placement.source_start == 0 {
        if let Some(task) = block.task {
            draw_checkbox(
                image,
                x.saturating_add(task.x),
                y.saturating_add(task.y),
                task.size,
                task.checked,
                palette.text,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::platforms::plugins::renderer) fn draw_table_fragment(
    image: &mut RgbaImage,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    block: &LayoutBlock,
    placement: &Placement,
    column_x: u32,
    destination_y: u32,
    palette: Palette,
) {
    let Some(table) = block.table.as_ref() else {
        return;
    };
    for row in table.rows.iter().filter(|row| {
        row.source_start >= placement.source_start && row.source_end <= placement.source_end
    }) {
        let row_y =
            destination_y.saturating_add(row.source_start.saturating_sub(placement.source_start));
        let row_height = row.source_end.saturating_sub(row.source_start);
        let background = if row.header {
            palette.table_header_background
        } else if row.stripe {
            palette.quote_background
        } else {
            palette.table_background
        };
        fill_rect(image, column_x, row_y, COLUMN_WIDTH, row_height, background);
        fill_rect(image, column_x, row_y, COLUMN_WIDTH, 1, palette.border);
        fill_rect(
            image,
            column_x,
            row_y.saturating_add(row_height.saturating_sub(1)),
            COLUMN_WIDTH,
            1,
            palette.border,
        );
        for cell in &row.cells {
            let cell_x = column_x.saturating_add(cell.x);
            fill_rect(image, cell_x, row_y, 1, row_height, palette.border);
            if cell.x.saturating_add(cell.width) == COLUMN_WIDTH {
                fill_rect(
                    image,
                    cell_x.saturating_add(cell.width.saturating_sub(1)),
                    row_y,
                    1,
                    row_height,
                    palette.border,
                );
            }
            draw_table_cell_text(
                image,
                font_system,
                swash_cache,
                cell,
                cell_x.saturating_add(TABLE_CELL_PADDING),
                row_y.saturating_add(TABLE_CELL_PADDING),
                cell_x.saturating_add(cell.width.saturating_sub(TABLE_CELL_PADDING)),
                row_y.saturating_add(row_height.saturating_sub(TABLE_CELL_PADDING)),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::platforms::plugins::renderer) fn draw_table_cell_text(
    image: &mut RgbaImage,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    cell: &LayoutTableCell,
    origin_x: u32,
    origin_y: u32,
    clip_x_end: u32,
    clip_y_end: u32,
) {
    for run in cell.buffer.layout_runs() {
        for (start_x, end_x) in inline_code_chip_ranges(run.glyphs) {
            let inset = (run.line_height * INLINE_CODE_CHIP_INSET_RATIO).max(2.0);
            let top = i64::from(origin_y) + (run.line_top + inset) as i64;
            let bottom = i64::from(origin_y) + (run.line_top + run.line_height - inset) as i64;
            let x0 = (i64::from(origin_x) + (start_x - INLINE_CODE_CHIP_PAD_X).floor() as i64)
                .max(i64::from(origin_x));
            let x1 = (i64::from(origin_x) + (end_x + INLINE_CODE_CHIP_PAD_X).ceil() as i64)
                .min(i64::from(clip_x_end));
            let bottom = bottom.min(i64::from(clip_y_end));
            let (Ok(x0), Ok(top)) = (u32::try_from(x0), u32::try_from(top)) else {
                continue;
            };
            if x1 <= i64::from(x0) || bottom <= i64::from(top) {
                continue;
            }
            fill_rect(
                image,
                x0,
                top,
                (x1 - i64::from(x0)) as u32,
                (bottom - i64::from(top)) as u32,
                cell.inline_code_background,
            );
        }
        for glyph in run.glyphs {
            if swash_cache.image_cache.len() >= MAX_CACHED_GLYPHS {
                swash_cache.image_cache.clear();
            }
            let physical = glyph.physical((0.0, 0.0), 1.0);
            let glyph_color = glyph.color_opt.unwrap_or(cell.default_color);
            swash_cache.with_pixels(
                font_system,
                physical.cache_key,
                glyph_color,
                |pixel_x, pixel_y, pixel_color| {
                    let global_x = i64::from(origin_x) + i64::from(physical.x) + i64::from(pixel_x);
                    let global_y = i64::from(origin_y)
                        + run.line_y as i64
                        + i64::from(physical.y)
                        + i64::from(pixel_y);
                    let (Ok(global_x), Ok(global_y)) =
                        (u32::try_from(global_x), u32::try_from(global_y))
                    else {
                        return;
                    };
                    if global_x < origin_x
                        || global_x >= clip_x_end
                        || global_y < origin_y
                        || global_y >= clip_y_end
                    {
                        return;
                    }
                    if let Some(destination) = image.get_pixel_mut_checked(global_x, global_y) {
                        destination.blend(&Rgba(pixel_color.as_rgba()));
                    }
                },
            );
        }
    }
}

pub(in crate::platforms::plugins::renderer) fn draw_checkbox(
    image: &mut RgbaImage,
    x: u32,
    y: u32,
    size: u32,
    checked: bool,
    color: [u8; 4],
) {
    if size < 4 {
        return;
    }
    fill_rect(image, x, y, size, 2, color);
    fill_rect(
        image,
        x,
        y.saturating_add(size.saturating_sub(2)),
        size,
        2,
        color,
    );
    fill_rect(image, x, y, 2, size, color);
    fill_rect(
        image,
        x.saturating_add(size.saturating_sub(2)),
        y,
        2,
        size,
        color,
    );
    if checked {
        draw_line(
            image,
            x.saturating_add(size / 5),
            y.saturating_add(size / 2),
            x.saturating_add(size * 2 / 5),
            y.saturating_add(size * 3 / 4),
            3,
            color,
        );
        draw_line(
            image,
            x.saturating_add(size * 2 / 5),
            y.saturating_add(size * 3 / 4),
            x.saturating_add(size * 4 / 5),
            y.saturating_add(size / 4),
            3,
            color,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::platforms::plugins::renderer) fn draw_line(
    image: &mut RgbaImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    width: u32,
    color: [u8; 4],
) {
    let dx = i64::from(x1).saturating_sub(i64::from(x0));
    let dy = i64::from(y1).saturating_sub(i64::from(y0));
    let steps = dx.unsigned_abs().max(dy.unsigned_abs()).max(1);
    for step in 0..=steps {
        let x = i64::from(x0).saturating_add(
            dx.saturating_mul(step as i64)
                .checked_div(steps as i64)
                .unwrap_or(0),
        );
        let y = i64::from(y0).saturating_add(
            dy.saturating_mul(step as i64)
                .checked_div(steps as i64)
                .unwrap_or(0),
        );
        let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else {
            continue;
        };
        fill_rect(image, x, y, width, width, color);
    }
}

pub(in crate::platforms::plugins::renderer) fn draw_text_fragment(
    image: &mut RgbaImage,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    block: &LayoutBlock,
    placement: &Placement,
    column_x: u32,
    destination_y: u32,
) {
    let Some(buffer) = block.buffer.as_ref() else {
        return;
    };
    let clip_x_end = column_x.saturating_add(COLUMN_WIDTH);
    let clip_y_end =
        destination_y.saturating_add(placement.source_end.saturating_sub(placement.source_start));
    for run in buffer.layout_runs() {
        let run_top = block.vertical_padding as f32 + run.line_top;
        let run_bottom = run_top + run.line_height;
        if run_bottom <= placement.source_start as f32 || run_top >= placement.source_end as f32 {
            continue;
        }
        for (start_x, end_x) in inline_code_chip_ranges(run.glyphs) {
            let inset = (run.line_height * INLINE_CODE_CHIP_INSET_RATIO).max(2.0);
            let top = (run_top + inset).max(placement.source_start as f32);
            let bottom = (run_bottom - inset).min(placement.source_end as f32);
            if bottom <= top {
                continue;
            }
            let global_y =
                i64::from(destination_y) + top as i64 - i64::from(placement.source_start);
            let x_base = i64::from(column_x) + i64::from(block.inset_left);
            let x0 = (x_base + (start_x - INLINE_CODE_CHIP_PAD_X).floor() as i64)
                .max(i64::from(column_x));
            let x1 = (x_base + (end_x + INLINE_CODE_CHIP_PAD_X).ceil() as i64)
                .min(i64::from(clip_x_end));
            let (Ok(x0), Ok(global_y)) = (u32::try_from(x0), u32::try_from(global_y)) else {
                continue;
            };
            if x1 <= i64::from(x0) {
                continue;
            }
            fill_rounded_rect(
                image,
                x0,
                global_y,
                (x1 - i64::from(x0)) as u32,
                (bottom - top) as u32,
                block.inline_code_background,
                6,
                true,
                true,
            );
        }
        for glyph in run.glyphs {
            if swash_cache.image_cache.len() >= MAX_CACHED_GLYPHS {
                swash_cache.image_cache.clear();
            }
            let physical = glyph.physical((0.0, 0.0), 1.0);
            let glyph_color = glyph.color_opt.unwrap_or(block.default_color);
            swash_cache.with_pixels(
                font_system,
                physical.cache_key,
                glyph_color,
                |pixel_x, pixel_y, pixel_color| {
                    let global_x = i64::from(column_x)
                        + i64::from(block.inset_left)
                        + i64::from(physical.x)
                        + i64::from(pixel_x);
                    let global_block_y = i64::from(block.vertical_padding)
                        + run.line_y as i64
                        + i64::from(physical.y)
                        + i64::from(pixel_y);
                    if global_block_y < i64::from(placement.source_start)
                        || global_block_y >= i64::from(placement.source_end)
                    {
                        return;
                    }
                    let global_y = i64::from(destination_y) + global_block_y
                        - i64::from(placement.source_start);
                    let (Ok(global_x), Ok(global_y)) =
                        (u32::try_from(global_x), u32::try_from(global_y))
                    else {
                        return;
                    };
                    if global_x < column_x
                        || global_x >= clip_x_end
                        || global_y < destination_y
                        || global_y >= clip_y_end
                    {
                        return;
                    }
                    if let Some(destination) = image.get_pixel_mut_checked(global_x, global_y) {
                        destination.blend(&Rgba(pixel_color.as_rgba()));
                    }
                },
            );
        }
    }
}

/// 圆角矩形填充:四角按半径裁剪,边缘 1px 用覆盖率混合抗锯齿。
/// `round_top`/`round_bottom` 支持跨页分片的代码块——只有块的真实首/尾
/// 分片才带圆角,中间分片保持直边无缝拼接。
#[allow(clippy::too_many_arguments)]
pub(in crate::platforms::plugins::renderer) fn fill_rounded_rect(
    image: &mut RgbaImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    color: [u8; 4],
    radius: u32,
    round_top: bool,
    round_bottom: bool,
) {
    let radius = radius.min(width / 2).min(height / 2);
    if radius == 0 {
        fill_rect(image, x, y, width, height, color);
        return;
    }
    let end_x = x.saturating_add(width).min(image.width());
    let end_y = y.saturating_add(height).min(image.height());
    let r = radius as f32;
    for py in y.min(end_y)..end_y {
        for px in x.min(end_x)..end_x {
            let local_x = (px - x) as f32;
            let local_y = (py - y) as f32;
            // 到最近圆角圆心的距离;不在角区=完全覆盖。
            let corner_x = if local_x < r {
                Some(r - 0.5)
            } else if local_x >= width as f32 - r {
                Some(width as f32 - r - 0.5)
            } else {
                None
            };
            let corner_y = if round_top && local_y < r {
                Some(r - 0.5)
            } else if round_bottom && local_y >= height as f32 - r {
                Some(height as f32 - r - 0.5)
            } else {
                None
            };
            let coverage = match (corner_x, corner_y) {
                (Some(cx), Some(cy)) => {
                    let distance = ((local_x - cx).powi(2) + (local_y - cy).powi(2)).sqrt();
                    (r - distance + 0.5).clamp(0.0, 1.0)
                }
                _ => 1.0,
            };
            if coverage <= 0.0 {
                continue;
            }
            if let Some(pixel) = image.get_pixel_mut_checked(px, py) {
                if coverage >= 1.0 {
                    *pixel = Rgba(color);
                } else {
                    let base = pixel.0;
                    let mix = |a: u8, b: u8| {
                        (f32::from(b) * coverage + f32::from(a) * (1.0 - coverage)).round() as u8
                    };
                    *pixel = Rgba([
                        mix(base[0], color[0]),
                        mix(base[1], color[1]),
                        mix(base[2], color[2]),
                        mix(base[3], color[3]),
                    ]);
                }
            }
        }
    }
}

pub(in crate::platforms::plugins::renderer) fn fill_rect(
    image: &mut RgbaImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    color: [u8; 4],
) {
    let end_x = x.saturating_add(width).min(image.width());
    let end_y = y.saturating_add(height).min(image.height());
    for py in y.min(end_y)..end_y {
        for px in x.min(end_x)..end_x {
            if let Some(pixel) = image.get_pixel_mut_checked(px, py) {
                *pixel = Rgba(color);
            }
        }
    }
}

pub(in crate::platforms::plugins::renderer) fn color(rgba: [u8; 4]) -> Color {
    Color::rgba(rgba[0], rgba[1], rgba[2], rgba[3])
}
