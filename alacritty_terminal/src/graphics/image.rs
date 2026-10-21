//! Bounded image decoding and resizing shared by the graphics protocols.

use std::io::Cursor;

use image::{ImageReader, Limits, RgbaImage, imageops};

use super::{ColorType, GraphicData, GraphicId, MAX_GRAPHIC_DIMENSIONS};

pub(super) const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_ENCODED_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;

pub(super) fn decode(data: &[u8]) -> Result<GraphicData, &'static str> {
    if data.len() > MAX_IMAGE_BYTES {
        return Err("E2BIG:Image data exceeds limit");
    }
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|_| "EINVAL:Unknown image format")?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_GRAPHIC_DIMENSIONS[0] as u32);
    limits.max_image_height = Some(MAX_GRAPHIC_DIMENSIONS[1] as u32);
    limits.max_alloc = Some(MAX_IMAGE_BYTES as u64);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| "EINVAL:Invalid or oversized image")?.into_rgba8();
    Ok(from_rgba(image))
}

pub(super) fn from_rgba(image: RgbaImage) -> GraphicData {
    GraphicData {
        id: GraphicId(0),
        width: image.width() as usize,
        height: image.height() as usize,
        color_type: ColorType::Rgba,
        is_opaque: image.pixels().all(|p| p.0[3] == 255),
        pixels: image.into_raw(),
    }
}

pub(super) fn rgba(graphic: &GraphicData) -> RgbaImage {
    let pixels = match graphic.color_type {
        ColorType::Rgba => graphic.pixels.clone(),
        ColorType::Rgb => {
            graphic.pixels.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect()
        },
    };
    RgbaImage::from_raw(graphic.width as u32, graphic.height as u32, pixels).unwrap()
}

/// Scale to fit a rectangle, keeping the aspect ratio unless explicitly disabled.
pub(super) fn resize(
    graphic: GraphicData,
    width: Option<usize>,
    height: Option<usize>,
    preserve: bool,
) -> Result<GraphicData, &'static str> {
    if width.is_some_and(|w| w == 0 || w > MAX_GRAPHIC_DIMENSIONS[0])
        || height.is_some_and(|h| h == 0 || h > MAX_GRAPHIC_DIMENSIONS[1])
    {
        return Err("EINVAL:Invalid display dimensions");
    }
    let (w, h) = match (width, height) {
        (None, None) => return Ok(graphic),
        (Some(w), None) => (w, (graphic.height * w / graphic.width).max(1)),
        (None, Some(h)) => ((graphic.width * h / graphic.height).max(1), h),
        (Some(w), Some(h)) if preserve => {
            let scale = (w as f64 / graphic.width as f64).min(h as f64 / graphic.height as f64);
            (
                (graphic.width as f64 * scale).round() as usize,
                (graphic.height as f64 * scale).round() as usize,
            )
        },
        (Some(w), Some(h)) => (w, h),
    };
    if w == 0 || h == 0 || w > MAX_GRAPHIC_DIMENSIONS[0] || h > MAX_GRAPHIC_DIMENSIONS[1] {
        return Err("EINVAL:Invalid display dimensions");
    }
    if w == graphic.width && h == graphic.height {
        return Ok(graphic);
    }
    Ok(from_rgba(imageops::resize(
        &rgba(&graphic),
        w as u32,
        h as u32,
        imageops::FilterType::Triangle,
    )))
}
