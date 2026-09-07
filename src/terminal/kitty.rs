use anyhow::{bail, Context, Result};
use base64::Engine;
use image::DynamicImage;
use std::io::{self, Write};
use std::path::Path;

const PLACEHOLDER: char = '\u{10eeee}';
const MAX_TRANSFER_DIMENSION: u32 = 2048;
const RAW_CHUNK_BYTES: usize = 3072;

pub fn is_native_kitty_terminal() -> bool {
    is_native_kitty(std::env::var("TERM").as_deref().unwrap_or_default())
}

/// 本进程是否已经往终端发过 Unicode 占位符图片。
///
/// 活动区靠这个决定腾地方时用哪种滚动:发过图之后 scrollback 里就有占位符行,
/// 受限区滚动会让 kitty 给它们留残影(见 `LiveReplTail::apply_output_frame`),
/// 得改整屏滚;纯文字会话保持原路径,零开销。
pub fn images_emitted() -> bool {
    IMAGES_EMITTED.load(std::sync::atomic::Ordering::Relaxed)
}

static IMAGES_EMITTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn is_native_kitty(term: &str) -> bool {
    term == "xterm-kitty"
}

pub fn supports_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "bmp" | "gif" | "jpg" | "jpeg" | "png" | "webp"
    )
}

pub fn print(path: &Path, requested_size: Option<&str>) -> Result<()> {
    let image = image::ImageReader::open(path)
        .with_context(|| format!("failed to open image {}", path.display()))?
        .with_guessed_format()
        .context("failed to detect image format")?
        .decode()
        .with_context(|| format!("failed to decode image {}", path.display()))?;
    let (terminal_cols, terminal_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (max_cols, max_rows) = parse_size(requested_size, terminal_cols, terminal_rows)?;
    if std::env::var_os("MIYU_IMAGE_TRACE").is_some() {
        let (cell_w, cell_h) = cell_pixel_size();
        let (cols, rows) = fit_cells(
            image.width(),
            image.height(),
            max_cols,
            max_rows,
            cell_w,
            cell_h,
        );
        let resized = resize_for_transfer(image.clone(), cols, rows, cell_w, cell_h);
        let line = format!(
            "终端 {terminal_cols}x{terminal_rows}  单元格 {cell_w}x{cell_h}  \
             请求 {requested_size:?}  框 {max_cols}x{max_rows}  \
             原图 {}x{}  →  网格 {cols}x{rows} = {}x{} px  \
             传输画布 {}x{}\n",
            image.width(),
            image.height(),
            u32::from(cols) * u32::from(cell_w),
            u32::from(rows) * u32::from(cell_h),
            resized.width(),
            resized.height(),
        );
        // 落文件而不是 stderr:live REPL 里 stderr 会直接糊在画面上。
        let path = std::path::Path::new(&std::env::var("HOME").unwrap_or_default())
            .join(".miyu/cache/logs/image-trace.log");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
    let sequence = kitty_sequence(&image, max_cols, max_rows)?;
    io::stdout().write_all(sequence.as_bytes())?;
    io::stdout().flush()?;
    Ok(())
}

/// 当前终端单元格像素尺寸(供公式渲染等调用方做尺寸账)。
pub(crate) fn cell_pixel_size() -> (u16, u16) {
    let (terminal_cols, terminal_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    terminal_cell_pixels(terminal_cols, terminal_rows)
}

/// 按调用方指定的 c×r 网格渲染成 Kitty Unicode-placeholder 序列
/// (占位行自带换行)。print_image 的"撑满可用区"与公式的"自然大小"
/// 都经由这里,只是网格怎么算不同。
pub(crate) fn kitty_sequence_with_grid(
    image: &DynamicImage,
    cols: u16,
    rows: u16,
) -> Result<String> {
    let (cell_width, cell_height) = cell_pixel_size();
    let resized = resize_for_transfer(image.clone(), cols, rows, cell_width, cell_height);
    let image_id = rand::random::<u32>() & 0x00ff_ffff;
    let image_id = image_id.max(1);
    let mut buffer = Vec::new();
    write_image(&mut buffer, &resized, image_id, cols, rows)?;
    Ok(String::from_utf8(buffer).context("kitty sequence is not utf-8")?)
}

/// 渲染成 Kitty 序列,撑满 max 框(看图语义,print_image 用)。
pub(crate) fn kitty_sequence(image: &DynamicImage, max_cols: u16, max_rows: u16) -> Result<String> {
    let (cell_width, cell_height) = cell_pixel_size();
    let (cols, rows) = fit_cells(
        image.width(),
        image.height(),
        max_cols,
        max_rows,
        cell_width,
        cell_height,
    );
    kitty_sequence_with_grid(image, cols, rows)
}

fn parse_size(
    requested: Option<&str>,
    terminal_cols: u16,
    terminal_rows: u16,
) -> Result<(u16, u16)> {
    let available_rows = terminal_rows.saturating_sub(1).max(1);
    let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok((terminal_cols.max(1), available_rows));
    };
    let Some((width, height)) = requested.split_once('x') else {
        bail!("invalid image size {requested:?}; expected WIDTHxHEIGHT")
    };
    let width = parse_dimension(width, "width")?.unwrap_or(terminal_cols.max(1));
    let height = parse_dimension(height, "height")?.unwrap_or(available_rows);
    // 上限跟着记号表走:每一格都要写行号+列号,超出表长就没有记号可用了。
    Ok((width.clamp(1, max_cells()), height.clamp(1, max_cells())))
}

fn parse_dimension(value: &str, name: &str) -> Result<Option<u16>> {
    if value.is_empty() {
        return Ok(None);
    }
    let value = value
        .parse::<u16>()
        .with_context(|| format!("invalid image {name}: {value:?}"))?;
    if value == 0 {
        bail!("image {name} must be greater than zero")
    }
    Ok(Some(value))
}

fn fit_cells(
    image_width: u32,
    image_height: u32,
    max_cols: u16,
    max_rows: u16,
    cell_width: u16,
    cell_height: u16,
) -> (u16, u16) {
    let image_width = u64::from(image_width.max(1));
    let image_height = u64::from(image_height.max(1));
    let max_cols = u64::from(max_cols.max(1));
    let max_rows = u64::from(max_rows.max(1));
    let cell_width = u64::from(cell_width.max(1));
    let cell_height = u64::from(cell_height.max(1));

    if image_width * max_rows * cell_height >= image_height * max_cols * cell_width {
        let rows = (image_height * max_cols * cell_width).div_ceil(image_width * cell_height);
        (max_cols as u16, rows.clamp(1, max_rows) as u16)
    } else {
        let cols = (image_width * max_rows * cell_height).div_ceil(image_height * cell_width);
        (cols.clamp(1, max_cols) as u16, max_rows as u16)
    }
}

fn resize_for_transfer(
    image: DynamicImage,
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
) -> DynamicImage {
    // 只缩不放。放大不会凭空长出细节，纯粹是拿显存和串口带宽换零收益：一张
    // 128×128 的表情包按网格放大到 624×624 就是 23.8 倍像素（显式 `300x200`
    // 时 172 倍）。而 `write_image` 发的 `c=`/`r=` 已经告诉 kitty 要铺满多少
    // 格，尺寸对不上时它本来就会自己缩放——放大交给终端的 GPU 做，是免费的。
    let width = u32::from(cols)
        .saturating_mul(u32::from(cell_width))
        .clamp(1, MAX_TRANSFER_DIMENSION)
        .min(image.width().max(1));
    let height = u32::from(rows)
        .saturating_mul(u32::from(cell_height))
        .clamp(1, MAX_TRANSFER_DIMENSION)
        .min(image.height().max(1));
    image.thumbnail(width, height)
}

fn write_image(
    output: &mut impl Write,
    image: &DynamicImage,
    image_id: u32,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let rgba = image.to_rgba8();
    let chunks = rgba.as_raw().chunks(RAW_CHUNK_BYTES);
    let chunk_count = chunks.len();
    IMAGES_EMITTED.store(true, std::sync::atomic::Ordering::Relaxed);
    for (index, chunk) in chunks.enumerate() {
        write!(output, "\x1b_Gq=2,")?;
        if index == 0 {
            write!(
                output,
                "i={image_id},a=T,U=1,f=32,t=d,s={},v={},c={cols},r={rows},",
                rgba.width(),
                rgba.height()
            )?;
        }
        let more = u8::from(index + 1 < chunk_count);
        write!(output, "m={more};")?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
        output.write_all(encoded.as_bytes())?;
        write!(output, "\x1b\\")?;
    }

    let [_, red, green, blue] = image_id.to_be_bytes();
    for row in 0..rows {
        let row_mark = row_diacritic(row).context("image is too tall for Kitty placeholders")?;
        write!(output, "\x1b[38;2;{red};{green};{blue}m")?;
        // 每一格都写全行号+列号，不靠 kitty 的连续性推断。
        //
        // 省略后续格子的记号本来是合法写法（kitty 会按前一格递推），但只
        // 在整行完好时成立。这一行被别的输出盖掉左边一截之后，带记号的
        // 首格没了，右边残存的裸占位符失去参照，kitty 拿错行号去渲染——
        // 屏幕上就是同一条切片在下方反复出现。写全了最坏也只是少半行。
        // chafa 就是每格都写全的。
        for col in 0..cols {
            let col_mark =
                row_diacritic(col).context("image is too wide for Kitty placeholders")?;
            write!(output, "{PLACEHOLDER}{row_mark}{col_mark}")?;
        }
        writeln!(output, "\x1b[39m")?;
    }
    Ok(())
}

fn row_diacritic(row: u16) -> Option<char> {
    ROW_DIACRITICS.get(usize::from(row)).copied()
}

/// 一张图最多能占多少行/列:占位符的行列号都取自同一张记号表。
fn max_cells() -> u16 {
    u16::try_from(ROW_DIACRITICS.len()).unwrap_or(u16::MAX)
}

#[cfg(unix)]
fn terminal_cell_pixels(cols: u16, rows: u16) -> (u16, u16) {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    let size = unsafe {
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) != 0 {
            return (10, 20);
        }
        size.assume_init()
    };
    if size.ws_xpixel == 0 || size.ws_ypixel == 0 || cols == 0 || rows == 0 {
        return (10, 20);
    }
    (
        size.ws_xpixel.checked_div(cols).unwrap_or(0).max(1),
        size.ws_ypixel.checked_div(rows).unwrap_or(0).max(1),
    )
}

#[cfg(not(unix))]
fn terminal_cell_pixels(_cols: u16, _rows: u16) -> (u16, u16) {
    (10, 20)
}

// Kitty defines this index table for placeholder row and column coordinates.
const ROW_DIACRITICS: &[char] = &[
    '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}', '\u{033f}',
    '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035b}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036a}', '\u{036b}', '\u{036c}', '\u{036d}', '\u{036e}', '\u{036f}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059c}', '\u{059d}', '\u{059e}', '\u{059f}', '\u{05a0}', '\u{05a1}',
    '\u{05a8}', '\u{05a9}', '\u{05ab}', '\u{05ac}', '\u{05af}', '\u{05c4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065a}', '\u{065b}', '\u{065d}', '\u{065e}', '\u{06d6}', '\u{06d7}', '\u{06d8}',
    '\u{06d9}', '\u{06da}', '\u{06db}', '\u{06dc}', '\u{06df}', '\u{06e0}', '\u{06e1}', '\u{06e2}',
    '\u{06e4}', '\u{06e7}', '\u{06e8}', '\u{06eb}', '\u{06ec}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073a}', '\u{073d}', '\u{073f}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074a}', '\u{07eb}', '\u{07ec}', '\u{07ed}', '\u{07ee}',
    '\u{07ef}', '\u{07f0}', '\u{07f1}', '\u{07f3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081b}', '\u{081c}', '\u{081d}', '\u{081e}', '\u{081f}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082a}', '\u{082b}', '\u{082c}',
    '\u{082d}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0f82}', '\u{0f83}', '\u{0f86}', '\u{0f87}',
    '\u{135d}', '\u{135e}', '\u{135f}', '\u{17dd}', '\u{193a}', '\u{1a17}', '\u{1a75}', '\u{1a76}',
    '\u{1a77}', '\u{1a78}', '\u{1a79}', '\u{1a7a}', '\u{1a7b}', '\u{1a7c}', '\u{1b6b}', '\u{1b6d}',
    '\u{1b6e}', '\u{1b6f}', '\u{1b70}', '\u{1b71}', '\u{1b72}', '\u{1b73}', '\u{1cd0}', '\u{1cd1}',
    '\u{1cd2}', '\u{1cda}', '\u{1cdb}', '\u{1ce0}', '\u{1dc0}', '\u{1dc1}', '\u{1dc3}', '\u{1dc4}',
    '\u{1dc5}', '\u{1dc6}', '\u{1dc7}', '\u{1dc8}', '\u{1dc9}', '\u{1dcb}', '\u{1dcc}', '\u{1dd1}',
    '\u{1dd2}', '\u{1dd3}', '\u{1dd4}', '\u{1dd5}', '\u{1dd6}', '\u{1dd7}', '\u{1dd8}', '\u{1dd9}',
    '\u{1dda}', '\u{1ddb}', '\u{1ddc}', '\u{1ddd}', '\u{1dde}', '\u{1ddf}', '\u{1de0}', '\u{1de1}',
    '\u{1de2}', '\u{1de3}', '\u{1de4}', '\u{1de5}', '\u{1de6}', '\u{1dfe}', '\u{20d0}', '\u{20d1}',
    '\u{20d4}', '\u{20d5}', '\u{20d6}', '\u{20d7}', '\u{20db}', '\u{20dc}', '\u{20e1}', '\u{20e7}',
    '\u{20e9}', '\u{20f0}',
];

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    #[test]
    fn native_kitty_detection_matches_cliphist_tui() {
        assert!(is_native_kitty("xterm-kitty"));
        assert!(!is_native_kitty("xterm-256color"));
        assert!(!is_native_kitty("wezterm"));
    }

    #[test]
    fn parses_bounded_and_partial_sizes() {
        assert_eq!(parse_size(Some("40x12"), 120, 40).unwrap(), (40, 12));
        assert_eq!(parse_size(Some("40x"), 120, 40).unwrap(), (40, 39));
        assert_eq!(parse_size(Some("x12"), 120, 40).unwrap(), (120, 12));
        assert!(parse_size(Some("40"), 120, 40).is_err());
    }

    #[test]
    fn fits_image_aspect_ratio_to_terminal_cells() {
        assert_eq!(fit_cells(400, 400, 40, 20, 10, 20), (40, 20));
        assert_eq!(fit_cells(1600, 900, 40, 20, 10, 20), (40, 12));
        assert_eq!(fit_cells(900, 1600, 40, 20, 10, 20), (23, 20));
    }

    #[test]
    fn emits_virtual_placement_and_text_cells() {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(2, 2, Rgba([1, 2, 3, 255])));
        let mut output = Vec::new();
        write_image(&mut output, &image, 0x010203, 2, 2).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("a=T,U=1,f=32,t=d,s=2,v=2,c=2,r=2,m=0;"));
        assert!(output.contains("\x1b[38;2;1;2;3m"));
        assert_eq!(output.matches(PLACEHOLDER).count(), 4);
        assert_eq!(output.matches('\n').count(), 2);
    }

    /// 回归：占位行只有首格带记号时，被别的输出盖掉左边一截，右边残存
    /// 的裸占位符就失去参照，kitty 拿错行号渲染——同一条切片在下方反复
    /// 出现。每格写全就最坏只是少半行。
    #[test]
    fn every_placeholder_cell_carries_its_own_row_and_column() {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(4, 4, Rgba([1, 2, 3, 255])));
        let mut output = Vec::new();
        write_image(&mut output, &image, 0x010203, 3, 2).unwrap();
        let output = String::from_utf8(output).unwrap();
        let cells = output.matches(PLACEHOLDER).count();
        assert_eq!(cells, 6);
        // 每个占位符后面都跟着两个记号,一个都不能省。
        for (row, row_mark) in ROW_DIACRITICS.iter().take(2).enumerate() {
            for (col, col_mark) in ROW_DIACRITICS.iter().take(3).enumerate() {
                let cell = format!("{PLACEHOLDER}{row_mark}{col_mark}");
                assert!(output.contains(&cell), "缺第 {row} 行第 {col} 列的记号");
            }
        }
    }

    /// 每格都要写列号之后，宽度就不能超过记号表长度了。
    #[test]
    fn requested_size_is_capped_to_the_diacritic_table() {
        let limit = max_cells();
        assert!(limit >= 200, "记号表太短:{limit}");
        let (cols, rows) = parse_size(Some("400x400"), 120, 40).unwrap();
        assert_eq!((cols, rows), (limit, limit));
        assert!(row_diacritic(cols.saturating_sub(1)).is_some());
    }

    #[test]
    fn chunks_large_transfers_and_supports_configured_height_limit() {
        assert!(ROW_DIACRITICS.len() >= 200);
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(40, 40, Rgba([1, 2, 3, 255])));
        let mut output = Vec::new();
        write_image(&mut output, &image, 0x010203, 1, 1).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("c=1,r=1,m=1;"));
        assert!(output.contains("\x1b\\\x1b_Gq=2,m=0;"));
    }
}

#[cfg(test)]
mod transfer_size_probe {
    use super::*;

    /// 改前的算法：网格多大就传多大，比原图大也照放。
    fn upscaling_resize(
        image: DynamicImage,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) -> DynamicImage {
        let width = u32::from(cols)
            .saturating_mul(u32::from(cell_width))
            .clamp(1, MAX_TRANSFER_DIMENSION);
        let height = u32::from(rows)
            .saturating_mul(u32::from(cell_height))
            .clamp(1, MAX_TRANSFER_DIMENSION);
        image.thumbnail(width, height)
    }

    /// 量尺：`cargo test --lib transfer_size_probe::transfer_canvas_size -- --ignored --nocapture`
    ///
    /// 方案 G3 说「300×200 cells 会 clamp 到 2048×2048 RGBA，约 16 MiB」。量下来
    /// 那个 clamp 基本不咬人——照片本来就在缩小。真花钱的是**小图被放大**。
    ///
    /// 「写进终端」是最终吐给 tty 的转义序列字节数（RGBA 走 base64，约 ×1.35）。
    #[test]
    #[ignore]
    fn transfer_canvas_size() {
        // 8×16 是常见 cell 尺寸；终端问不出来时代码也回退到它
        let (cell_w, cell_h) = (8u16, 16u16);
        println!(
            "\n  {:<24}{:<12}{:>19}{:>19}{:>10}",
            "场景", "原图", "改前 画布/写入", "改后 画布/写入", "省"
        );
        for (label, image_w, image_h, term_cols, term_rows, requested) in [
            ("表情包 终端 120×40", 128u32, 128u32, 120u16, 40u16, None),
            ("照片 终端 120×40", 1920, 1080, 120, 40, None),
            ("照片 终端 240×60", 1920, 1080, 240, 60, None),
            ("照片 显式 300×200", 1920, 1080, 120, 40, Some("300x200")),
            ("小图 显式 300×200", 128, 128, 120, 40, Some("300x200")),
            // 公式走的是 render::math，它**故意**造 2× 网格的画布让传输层折半
            // （见那边「传输层 thumbnail 恰好折半」的注释），网格由它自己定，
            // 不经过 fit_cells。这里照它的真实形态摆，验证改动没碰到这条路。
            ("公式 2× 画布 100×3 格", 1600, 96, 120, 40, Some("math")),
        ] {
            let image = DynamicImage::new_rgba8(image_w, image_h);
            let (cols, rows) = if requested == Some("math") {
                (100u16, 3u16)
            } else {
                let (max_cols, max_rows) = parse_size(requested, term_cols, term_rows).unwrap();
                fit_cells(image_w, image_h, max_cols, max_rows, cell_w, cell_h)
            };

            let mut written = Vec::new();
            for (index, sent) in [
                upscaling_resize(image.clone(), cols, rows, cell_w, cell_h),
                resize_for_transfer(image.clone(), cols, rows, cell_w, cell_h),
            ]
            .into_iter()
            .enumerate()
            {
                let mut buffer = Vec::new();
                write_image(&mut buffer, &sent, 1, cols, rows).unwrap();
                written.push((
                    format!("{}x{}", sent.width(), sent.height()),
                    buffer.len(),
                    index,
                ));
            }
            let (before_size, before_bytes, _) = &written[0];
            let (after_size, after_bytes, _) = &written[1];
            println!(
                "  {label:<24}{:<12}{:>19}{:>19}{:>9.0}%",
                format!("{image_w}x{image_h}"),
                format!("{before_size} {:.2}MiB", *before_bytes as f64 / 1048576.0),
                format!("{after_size} {:.2}MiB", *after_bytes as f64 / 1048576.0),
                (1.0 - *after_bytes as f64 / *before_bytes as f64) * 100.0,
            );
        }
    }

    /// 传输画布永远不该超过原图——放大是纯浪费，交给终端做。
    #[test]
    fn transfer_never_upscales() {
        for (width, height) in [(1u32, 1u32), (16, 16), (128, 128), (900, 96), (4000, 3000)] {
            let sent = resize_for_transfer(DynamicImage::new_rgba8(width, height), 300, 200, 8, 16);
            assert!(
                sent.width() <= width && sent.height() <= height,
                "{width}x{height} 被放大成了 {}x{}",
                sent.width(),
                sent.height()
            );
        }
    }

    /// 但该缩的还得缩：大图仍要压进网格，否则显存白花。
    #[test]
    fn transfer_still_downscales_large_images() {
        let sent = resize_for_transfer(DynamicImage::new_rgba8(4000, 3000), 120, 39, 8, 16);
        assert!(sent.width() <= 960 && sent.height() <= 624);
    }
}
