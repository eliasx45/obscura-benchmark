//! Decode and compare WPT reftest screenshots using WPT fuzzy semantics.

use std::io::Cursor;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use png::{ColorType, Decoder, Transformations};

use crate::manifest::Fuzzy;

pub struct Difference {
    pub equal: bool,
    pub max_difference: u64,
    pub different_pixels: u64,
}

struct Image {
    width: u32,
    height: u32,
    color_type: ColorType,
    bytes: Vec<u8>,
}

pub fn compare_screenshots(left: &str, right: &str, fuzzy: Fuzzy) -> Result<Difference> {
    let engine = base64::engine::general_purpose::STANDARD;
    let left = engine.decode(left).context("decode test screenshot")?;
    let right = engine
        .decode(right)
        .context("decode reference screenshot")?;
    compare_png(&left, &right, fuzzy)
}

fn compare_png(left: &[u8], right: &[u8], fuzzy: Fuzzy) -> Result<Difference> {
    let left = decode_png(left)?;
    let right = decode_png(right)?;
    if left.width != right.width || left.height != right.height {
        return Ok(Difference {
            equal: false,
            max_difference: 255,
            different_pixels: u64::from(left.width.max(right.width))
                * u64::from(left.height.max(right.height)),
        });
    }

    let mut max_difference = 0u64;
    let mut different_pixels = 0u64;
    let pixels = u64::from(left.width) * u64::from(left.height);
    for index in 0..pixels as usize {
        let left_rgb = rgb(&left, index)?;
        let right_rgb = rgb(&right, index)?;
        let difference = left_rgb
            .into_iter()
            .zip(right_rgb)
            .map(|(a, b)| u64::from(a.abs_diff(b)))
            .max()
            .unwrap_or(0);
        if difference != 0 {
            different_pixels += 1;
        }
        max_difference = max_difference.max(difference);
    }

    let exact = fuzzy == Fuzzy::default();
    let equal = if exact {
        max_difference == 0 && different_pixels == 0
    } else {
        (different_pixels == 0 && fuzzy.different_pixels[0] == 0)
            || (max_difference == 0 && fuzzy.max_difference[0] == 0)
            || (fuzzy.max_difference[0] <= max_difference
                && max_difference <= fuzzy.max_difference[1]
                && fuzzy.different_pixels[0] <= different_pixels
                && different_pixels <= fuzzy.different_pixels[1])
    };
    Ok(Difference {
        equal,
        max_difference,
        different_pixels,
    })
}

fn decode_png(bytes: &[u8]) -> Result<Image> {
    let mut decoder = Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().context("read PNG header")?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| anyhow!("PNG output is too large"))?;
    let mut output = vec![0; size];
    let info = reader.next_frame(&mut output).context("decode PNG frame")?;
    output.truncate(info.buffer_size());
    Ok(Image {
        width: info.width,
        height: info.height,
        color_type: info.color_type,
        bytes: output,
    })
}

fn rgb(image: &Image, index: usize) -> Result<[u8; 3]> {
    let samples = image.color_type.samples();
    let offset = index
        .checked_mul(samples)
        .ok_or_else(|| anyhow!("PNG pixel offset overflow"))?;
    let pixel = image
        .bytes
        .get(offset..offset + samples)
        .ok_or_else(|| anyhow!("PNG pixel data is truncated"))?;
    Ok(match image.color_type {
        ColorType::Grayscale | ColorType::GrayscaleAlpha => [pixel[0]; 3],
        ColorType::Rgb | ColorType::Rgba => [pixel[0], pixel[1], pixel[2]],
        ColorType::Indexed => return Err(anyhow!("indexed PNG was not expanded")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(pixel: [u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&pixel).unwrap();
        drop(writer);
        bytes
    }

    #[test]
    fn exact_comparison_uses_rgb_and_ignores_alpha() {
        let result = compare_png(
            &png([10, 20, 30, 0]),
            &png([10, 20, 30, 255]),
            Fuzzy::default(),
        )
        .unwrap();
        assert!(result.equal);
        assert_eq!(result.different_pixels, 0);
    }

    #[test]
    fn fuzzy_comparison_checks_both_ranges() {
        let fuzzy = Fuzzy {
            max_difference: [0, 2],
            different_pixels: [0, 1],
        };
        let result = compare_png(&png([10, 20, 30, 255]), &png([12, 20, 30, 255]), fuzzy).unwrap();
        assert!(result.equal);
        assert_eq!(result.max_difference, 2);
        assert_eq!(result.different_pixels, 1);
    }
}
