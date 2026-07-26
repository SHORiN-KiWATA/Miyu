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
    let (cell_width, cell_height) = terminal_cell_pixels(terminal_cols, terminal_rows);
    let (cols, rows) = fit_cells(
        image.width(),
        image.height(),
        max_cols,
        max_rows,
        cell_width,
        cell_height,
    );
    let image = resize_for_transfer(image, cols, rows, cell_width, cell_height);
    let image_id = rand::random::<u32>() & 0x00ff_ffff;
    let image_id = image_id.max(1);
    write_image(&mut io::stdout(), &image, image_id, cols, rows)?;
    io::stdout().flush()?;
    Ok(())
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
    Ok((width.clamp(1, 300), height.clamp(1, 200)))
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
    let width = u32::from(cols)
        .saturating_mul(u32::from(cell_width))
        .clamp(1, MAX_TRANSFER_DIMENSION);
    let height = u32::from(rows)
        .saturating_mul(u32::from(cell_height))
        .clamp(1, MAX_TRANSFER_DIMENSION);
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
        write!(
            output,
            "\x1b[38;2;{red};{green};{blue}m{PLACEHOLDER}{row_mark}{}",
            ROW_DIACRITICS[0]
        )?;
        for _ in 1..cols {
            write!(output, "{PLACEHOLDER}")?;
        }
        writeln!(output, "\x1b[39m")?;
    }
    Ok(())
}

fn row_diacritic(row: u16) -> Option<char> {
    ROW_DIACRITICS.get(usize::from(row)).copied()
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
