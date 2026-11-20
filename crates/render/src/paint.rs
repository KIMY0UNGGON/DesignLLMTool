//! Colors and raster image decoding shared by the encoders.
use designllm_contracts::{DltError, DltResult};
use std::io::Cursor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba { pub r: u8, pub g: u8, pub b: u8, pub a: u8 }

/// Parse a style color. `Ok(None)` = no paint (none/transparent/empty). `Err(())` = unsupported.
pub fn parse_color(value: &str) -> Result<Option<Rgba>, ()> {
    let v = value.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") || v.eq_ignore_ascii_case("transparent") { return Ok(None); }
    if let Some(hex) = v.strip_prefix('#') {
        let digit = |s: &str| u8::from_str_radix(s, 16).map_err(|_| ());
        return match hex.len() {
            3 => {
                let c: Vec<u8> = hex.chars().map(|ch| ch.to_digit(16).map(|d| (d * 17) as u8).ok_or(())).collect::<Result<_, _>>()?;
                Ok(Some(Rgba { r: c[0], g: c[1], b: c[2], a: 255 }))
            }
            6 => Ok(Some(Rgba { r: digit(&hex[0..2])?, g: digit(&hex[2..4])?, b: digit(&hex[4..6])?, a: 255 })),
            8 => Ok(Some(Rgba { r: digit(&hex[0..2])?, g: digit(&hex[2..4])?, b: digit(&hex[4..6])?, a: digit(&hex[6..8])? })),
            _ => Err(()),
        };
    }
    let named = match v.to_ascii_lowercase().as_str() {
        "black" => (0, 0, 0), "white" => (255, 255, 255), "red" => (255, 0, 0), "green" => (0, 128, 0),
        "blue" => (0, 0, 255), "gray" | "grey" => (128, 128, 128), "yellow" => (255, 255, 0), _ => return Err(()),
    };
    Ok(Some(Rgba { r: named.0, g: named.1, b: named.2, a: 255 }))
}

pub fn hex(c: Rgba) -> String { format!("#{:02X}{:02X}{:02X}", c.r, c.g, c.b) }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind { Png, Jpeg, Bmp, Gif, Webp, Other }

pub fn sniff(bytes: &[u8]) -> ImageKind {
    if bytes.starts_with(&designllm_contracts::media_header::PNG_SIGNATURE) { ImageKind::Png }
    else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) { ImageKind::Jpeg }
    else if bytes.starts_with(b"BM") && bytes.len() > 26 { ImageKind::Bmp }
    else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") { ImageKind::Gif }
    else if bytes.starts_with(b"RIFF") && bytes.get(8..12).is_some_and(|signature| signature == b"WEBP") { ImageKind::Webp }
    else { ImageKind::Other }
}

/// Straight (non-premultiplied) RGBA8 image. `warnings` lists tolerated defects.
pub struct Decoded { pub width: u32, pub height: u32, pub rgba: Vec<u8>, pub warnings: Vec<String> }

impl Decoded {
    pub fn has_alpha(&self) -> bool { self.rgba.chunks_exact(4).any(|p| p[3] != 255) }
}

const MAX_PIXELS: u64 = 64 * 1024 * 1024;

fn within_pixel_limit(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_PIXELS
}

pub fn decode(bytes: &[u8]) -> DltResult<Decoded> {
    match sniff(bytes) {
        ImageKind::Png => decode_png(bytes),
        ImageKind::Jpeg => decode_jpeg(bytes),
        ImageKind::Bmp => decode_bmp(bytes),
        ImageKind::Gif => decode_gif(bytes),
        ImageKind::Webp => decode_webp(bytes),
        ImageKind::Other => Err(DltError::unsupported("Unknown image format (only PNG, JPEG, BMP, GIF and static WebP are rendered)")),
    }
}

fn gif_logical_dimensions(bytes: &[u8]) -> DltResult<(u32, u32)> {
    if bytes.len() < 10 || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Err(DltError::invalid("GIF header is truncated or invalid"));
    }
    let width = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
    let height = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
    if !within_pixel_limit(width, height) {
        return Err(DltError::invalid("GIF exceeds the 64 megapixel limit"));
    }
    Ok((width, height))
}

fn decode_gif(bytes: &[u8]) -> DltResult<Decoded> {
    use image::ImageDecoder;

    let dimensions = gif_logical_dimensions(bytes)?;
    let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
        .map_err(|error| DltError::invalid(format!("GIF decode failed: {error}")))?;
    if decoder.dimensions() != dimensions {
        return Err(DltError::invalid("GIF dimensions changed while reading its header"));
    }
    // Mirror image 0.25.10's 50 MB GIF frame limit so oversized partial-frame buffers fail before allocation.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(dimensions.0);
    limits.max_image_height = Some(dimensions.1);
    limits.max_alloc = Some(50_000_000);
    decoder.set_limits(limits)
        .map_err(|error| DltError::invalid(format!("GIF dimensions are unsupported: {error}")))?;
    // shortcut: GIF is flattened to its first frame; upgrade when HTML and static exports share animation timing.
    decode_rgb_decoder(decoder, "GIF").map_err(|error| {
        if error.message.to_ascii_lowercase().contains("memory") {
            DltError::invalid("GIF frame exceeds the decoder's 50,000,000-byte per-frame limit")
        } else {
            error
        }
    })
}

fn webp_static_dimensions(bytes: &[u8]) -> DltResult<(u32, u32)> {
    let invalid = || DltError::invalid("WebP header is truncated or invalid");
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(invalid());
    }
    let riff_end = usize::try_from(u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| invalid())?))
        .ok().and_then(|size| size.checked_add(8)).filter(|end| (12..=bytes.len()).contains(end))
        .ok_or_else(invalid)?;
    // image-webp 0.2.4's VP8X scan can read ten bytes past the declared RIFF end.
    if riff_end != bytes.len() { return Err(invalid()); }
    let mut offset = 12usize;
    let mut canvas = None;
    let mut frame = None;
    while offset < riff_end {
        let header_end = offset.checked_add(8).filter(|end| *end <= riff_end).ok_or_else(invalid)?;
        let tag = &bytes[offset..offset + 4];
        let chunk_len = usize::try_from(u32::from_le_bytes(bytes[offset + 4..header_end].try_into().map_err(|_| invalid())?))
            .map_err(|_| invalid())?;
        let payload_start = header_end;
        let payload_end = payload_start.checked_add(chunk_len).filter(|end| *end <= riff_end).ok_or_else(invalid)?;
        let chunk_end = payload_end.checked_add(chunk_len & 1).filter(|end| *end <= riff_end).ok_or_else(invalid)?;
        let payload = &bytes[payload_start..payload_end];
        match tag {
            b"ANIM" | b"ANMF" => return Err(DltError::unsupported("Animated WebP images are not rendered; use a static WebP image")),
            b"VP8X" => {
                if payload.len() < 10 { return Err(invalid()); }
                if payload[0] & 0b10 != 0 {
                    return Err(DltError::unsupported("Animated WebP images are not rendered; use a static WebP image"));
                }
                let width = 1 + u32::from(payload[4]) + (u32::from(payload[5]) << 8) + (u32::from(payload[6]) << 16);
                let height = 1 + u32::from(payload[7]) + (u32::from(payload[8]) << 8) + (u32::from(payload[9]) << 16);
                if !within_pixel_limit(width, height) { return Err(DltError::invalid("WebP canvas exceeds the 64 megapixel limit")); }
                if canvas.replace((width, height)).is_some() { return Err(invalid()); }
            }
            b"VP8 " => {
                if payload.len() < 10 || payload[0] & 1 != 0 || payload[3..6] != [0x9d, 0x01, 0x2a] {
                    return Err(invalid());
                }
                let width = u16::from_le_bytes([payload[6], payload[7]]) as u32 & 0x3fff;
                let height = u16::from_le_bytes([payload[8], payload[9]]) as u32 & 0x3fff;
                if !within_pixel_limit(width, height) { return Err(DltError::invalid("WebP frame exceeds the 64 megapixel limit")); }
                if frame.replace((width, height)).is_some() { return Err(invalid()); }
            }
            b"VP8L" => {
                if payload.len() < 5 || payload[0] != 0x2f { return Err(invalid()); }
                let header = u32::from_le_bytes(payload[1..5].try_into().map_err(|_| invalid())?);
                let width = 1 + (header & 0x3fff);
                let height = 1 + ((header >> 14) & 0x3fff);
                if !within_pixel_limit(width, height) { return Err(DltError::invalid("WebP frame exceeds the 64 megapixel limit")); }
                if frame.replace((width, height)).is_some() { return Err(invalid()); }
            }
            _ => {}
        }
        offset = chunk_end;
    }
    if offset != riff_end { return Err(invalid()); }
    let frame = frame.ok_or_else(invalid)?;
    let dimensions = canvas.unwrap_or(frame);
    if dimensions != frame { return Err(DltError::invalid("WebP frame dimensions do not match the canvas")); }
    Ok(dimensions)
}

fn decode_webp(bytes: &[u8]) -> DltResult<Decoded> {
    use image::ImageDecoder;

    let dimensions = webp_static_dimensions(bytes)?;
    let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
        .map_err(|error| DltError::invalid(format!("WebP decode failed: {error}")))?;
    if decoder.dimensions() != dimensions {
        return Err(DltError::invalid("WebP dimensions changed while reading its header"));
    }
    decode_rgb_decoder(decoder, "WebP")
}

fn decode_rgb_decoder(decoder: impl image::ImageDecoder, format: &str) -> DltResult<Decoded> {
    let (width, height) = decoder.dimensions();
    if !within_pixel_limit(width, height) {
        return Err(DltError::invalid(format!("{format} exceeds the 64 megapixel limit")));
    }
    let channels = match decoder.color_type() {
        image::ColorType::Rgb8 => 3usize,
        image::ColorType::Rgba8 => 4usize,
        _ => return Err(DltError::unsupported(format!("{format} returned an unsupported pixel layout"))),
    };
    let pixels = usize::try_from(u64::from(width) * u64::from(height))
        .map_err(|_| DltError::invalid(format!("{format} image dimensions overflow memory limits")))?;
    let raw_size = pixels.checked_mul(channels)
        .ok_or_else(|| DltError::invalid(format!("{format} image dimensions overflow memory limits")))?;
    let rgba_size = pixels.checked_mul(4)
        .ok_or_else(|| DltError::invalid(format!("{format} image dimensions overflow memory limits")))?;
    if decoder.total_bytes() != raw_size as u64 {
        return Err(DltError::invalid(format!("{format} decoder reported an unexpected pixel layout")));
    }
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(rgba_size)
        .map_err(|_| DltError::invalid(format!("{format} image is too large to decode")))?;
    rgba.resize(raw_size, 0);
    decoder.read_image(&mut rgba)
        .map_err(|error| DltError::invalid(format!("{format} decode failed: {error}")))?;
    if channels == 3 {
        rgba.resize(rgba_size, 0);
        for pixel in (0..pixels).rev() {
            let source = pixel * 3;
            let target = pixel * 4;
            rgba.copy_within(source..source + 3, target);
            rgba[target + 3] = 255;
        }
    }
    Ok(Decoded { width, height, rgba, warnings: vec![] })
}

fn decode_png(bytes: &[u8]) -> DltResult<Decoded> {
    match decode_png_with(bytes, false) {
        Ok(d) => Ok(d),
        Err(strict) => {
            // Chunk CRC / Adler-32 mismatches only: decode the data anyway and report it.
            let message = strict.to_string();
            if !message.contains("CRC") && !message.to_ascii_lowercase().contains("checksum") && !message.contains("Adler") {
                return Err(DltError::invalid(format!("PNG decode failed: {message}")));
            }
            let mut d = decode_png_with(bytes, true).map_err(|e| DltError::invalid(format!("PNG decode failed: {e}")))?;
            d.warnings.push(format!("PNG checksum mismatch ignored ({message})"));
            Ok(d)
        }
    }
}

fn decode_png_with(bytes: &[u8], ignore_checksums: bool) -> Result<Decoded, png::DecodingError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    decoder.ignore_checksums(ignore_checksums);
    let mut reader = decoder.read_info()?;
    // Check untrusted dimensions before asking for or allocating a frame buffer.
    let header = reader.info();
    if !within_pixel_limit(header.width, header.height) { return Err(png::DecodingError::LimitsExceeded); }
    let size = reader.output_buffer_size().ok_or_else(|| png::DecodingError::LimitsExceeded)?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf)?;
    let rgba_row = usize::try_from(info.width).ok().and_then(|width| width.checked_mul(4));
    let rgba_size = rgba_row.and_then(|row| usize::try_from(info.height).ok()?.checked_mul(row));
    if info.color_type == png::ColorType::Rgba
        && info.bit_depth == png::BitDepth::Eight
        && rgba_row == Some(info.line_size)
        && rgba_size == Some(buf.len())
    {
        return Ok(Decoded { width: info.width, height: info.height, rgba: buf, warnings: vec![] });
    }
    let px = (info.width * info.height) as usize;
    let mut rgba = Vec::with_capacity(px * 4);
    for row in buf.chunks(info.line_size).take(info.height as usize) {
        match info.color_type {
            png::ColorType::Grayscale => for &v in &row[..info.width as usize] { rgba.extend_from_slice(&[v, v, v, 255]); },
            png::ColorType::GrayscaleAlpha => for p in row[..info.width as usize * 2].chunks_exact(2) { rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]); },
            png::ColorType::Rgb => for p in row[..info.width as usize * 3].chunks_exact(3) { rgba.extend_from_slice(&[p[0], p[1], p[2], 255]); },
            png::ColorType::Rgba => rgba.extend_from_slice(&row[..info.width as usize * 4]),
            png::ColorType::Indexed => return Err(png::DecodingError::LimitsExceeded),
        }
    }
    Ok(Decoded { width: info.width, height: info.height, rgba, warnings: vec![] })
}

fn decode_jpeg(bytes: &[u8]) -> DltResult<Decoded> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA).set_max_width(1 << 15).set_max_height(1 << 15);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(bytes, options);
    decoder.decode_headers().map_err(|e| DltError::invalid(format!("JPEG decode failed: {e:?}")))?;
    let (w, h) = decoder.dimensions().ok_or_else(|| DltError::invalid("JPEG has no dimensions"))?;
    let (width, height) = (w as u32, h as u32);
    if !within_pixel_limit(width, height) { return Err(DltError::invalid("JPEG exceeds the 64 megapixel limit")); }
    let pixels = decoder.decode().map_err(|e| DltError::invalid(format!("JPEG decode failed: {e:?}")))?;
    if pixels.len() != w * h * 4 { return Err(DltError::invalid("JPEG decoder returned an unexpected pixel layout")); }
    Ok(Decoded { width, height, rgba: pixels, warnings: vec![] })
}

/// JPEG frame size and component count from the SOF marker (for DCT pass-through).
pub fn jpeg_info(bytes: &[u8]) -> Option<(u32, u32, u8)> {
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF { i += 1; continue; }
        let marker = bytes[i + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) || marker == 0xFF { i += if marker == 0xFF { 1 } else { 2 }; continue; }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            if i + 9 >= bytes.len() { return None; }
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            return Some((w, h, bytes[i + 9]));
        }
        i += 2 + len;
    }
    None
}

/// Image size in pixels without full decoding when possible.
pub fn pixel_size(bytes: &[u8]) -> Option<(u32, u32)> {
    match sniff(bytes) {
        ImageKind::Png if bytes.len() >= 24 => Some((u32::from_be_bytes(bytes[16..20].try_into().ok()?), u32::from_be_bytes(bytes[20..24].try_into().ok()?))),
        ImageKind::Jpeg => jpeg_info(bytes).map(|(w, h, _)| (w, h)),
        ImageKind::Bmp => bmp_header(bytes).ok().map(|h| (h.width, h.height)),
        ImageKind::Gif => gif_logical_dimensions(bytes).ok(),
        ImageKind::Webp => webp_static_dimensions(bytes).ok(),
        _ => None,
    }
}

/// Returns the non-default EXIF orientation from a JPEG APP1 segment.
/// `None` means no orientation tag or orientation 1; values 2..=8 require an explicit transform.
/// Malformed EXIF metadata is rejected so callers can make the same crop decision.
pub fn jpeg_exif_orientation(bytes: &[u8]) -> DltResult<Option<u8>> {
    if sniff(bytes) != ImageKind::Jpeg {
        return Ok(None);
    }
    let invalid = || DltError::invalid("JPEG EXIF orientation metadata is malformed");
    let mut offset = 2usize;
    let mut found = None;
    while offset < bytes.len() {
        if bytes[offset] != 0xFF {
            return Err(invalid());
        }
        while bytes.get(offset) == Some(&0xFF) {
            offset += 1;
        }
        let marker = *bytes.get(offset).ok_or_else(invalid)?;
        offset += 1;
        if matches!(marker, 0xDA | 0xD9) {
            break;
        }
        if matches!(marker, 0xD8 | 0x01 | 0xD0..=0xD7) {
            continue;
        }
        let length_bytes = bytes.get(offset..offset + 2).ok_or_else(invalid)?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return Err(invalid());
        }
        let segment_end = offset.checked_add(length).filter(|end| *end <= bytes.len()).ok_or_else(invalid)?;
        let payload = &bytes[offset + 2..segment_end];
        if marker == 0xE1 && payload.starts_with(b"Exif") {
            if !payload.starts_with(b"Exif\0\0") {
                return Err(invalid());
            }
            let orientation = tiff_orientation(&payload[6..]).ok_or_else(invalid)?;
            if found.is_some_and(|previous| previous != orientation) { return Err(invalid()); }
            found = Some(orientation);
        }
        offset = segment_end;
    }
    match found {
        Some(1) | None => Ok(None),
        Some(orientation) if (2..=8).contains(&orientation) => Ok(Some(orientation)),
        Some(_) => Err(invalid()),
    }
}

pub(crate) fn validate_jpeg_crop_orientation(bytes: &[u8]) -> DltResult<()> {
    if let Some(orientation) = jpeg_exif_orientation(bytes)? {
        return Err(DltError::unsupported(format!(
            "JPEG EXIF orientation {orientation} is unsupported for pixel cropping"
        )));
    }
    Ok(())
}

pub(crate) fn crop_diagnostic_code(error: &DltError) -> Option<&'static str> {
    if error.message.starts_with("JPEG EXIF orientation") {
        Some("IMAGE_CROP_ORIENTATION_UNSUPPORTED")
    } else if error.message.starts_with("image crop") {
        Some("IMAGE_CROP_INVALID")
    } else {
        None
    }
}

fn tiff_orientation(bytes: &[u8]) -> Option<u8> {
    let endian = if bytes.get(..2)? == b"II" { true } else if bytes.get(..2)? == b"MM" { false } else { return None };
    let u16_at = |at: usize| -> Option<u16> {
        let pair: [u8; 2] = bytes.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if endian { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let quad: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if endian { u32::from_le_bytes(quad) } else { u32::from_be_bytes(quad) })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let count = usize::from(u16_at(ifd)?);
    let entries_start = ifd.checked_add(2)?;
    let entries_bytes = count.checked_mul(12)?;
    let entries_end = entries_start.checked_add(entries_bytes)?;
    let next_ifd_end = entries_end.checked_add(4)?;
    if next_ifd_end > bytes.len() {
        return None;
    }
    let next_ifd = usize::try_from(u32_at(entries_end)?).ok()?;
    if next_ifd != 0 && next_ifd.checked_add(2).is_none_or(|end| end > bytes.len()) {
        return None;
    }
    for index in 0..count {
        let at = entries_start.checked_add(index.checked_mul(12)?)?;
        if u16_at(at)? != 0x0112 {
            continue;
        }
        if u16_at(at.checked_add(2)?)? != 3 || u32_at(at.checked_add(4)?)? != 1 {
            return None;
        }
        let orientation = u16_at(at.checked_add(8)?)?;
        return (1..=8).contains(&orientation).then_some(orientation as u8);
    }
    Some(1)
}

/// Crops decoded RGBA pixels without changing the source asset. The returned image owns only the
/// selected rows; a full-image rectangle reuses the decoded buffer unchanged.
pub(crate) fn crop_decoded_image(mut image: Decoded, crop: designllm_contracts::PixelRect) -> DltResult<Decoded> {
    let right = crop.x.checked_add(crop.width).filter(|right| *right <= image.width);
    let bottom = crop.y.checked_add(crop.height).filter(|bottom| *bottom <= image.height);
    if crop.width == 0 || crop.height == 0 || right.is_none() || bottom.is_none() {
        return Err(DltError::invalid("image crop is empty or outside the source pixels"));
    }
    let expected = u64::from(image.width).checked_mul(u64::from(image.height))
        .and_then(|pixels| pixels.checked_mul(4)).and_then(|bytes| usize::try_from(bytes).ok());
    if expected != Some(image.rgba.len()) {
        return Err(DltError::invalid("decoded image pixel buffer has an unexpected size"));
    }
    if crop.x == 0 && crop.y == 0 && crop.width == image.width && crop.height == image.height {
        return Ok(image);
    }
    let row_bytes = usize::try_from(u64::from(crop.width) * 4)
        .map_err(|_| DltError::invalid("image crop dimensions overflow memory limits"))?;
    let size = row_bytes.checked_mul(crop.height as usize)
        .ok_or_else(|| DltError::invalid("image crop dimensions overflow memory limits"))?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(size).map_err(|_| DltError::invalid("image crop is too large to render"))?;
    let source_stride = usize::try_from(u64::from(image.width) * 4)
        .map_err(|_| DltError::invalid("decoded image dimensions overflow memory limits"))?;
    let source_x = usize::try_from(u64::from(crop.x) * 4)
        .map_err(|_| DltError::invalid("image crop dimensions overflow memory limits"))?;
    for row in crop.y..bottom.expect("validated above") {
        let start = (row as usize).checked_mul(source_stride).and_then(|offset| offset.checked_add(source_x))
            .ok_or_else(|| DltError::invalid("image crop row offset overflowed"))?;
        let end = start.checked_add(row_bytes).ok_or_else(|| DltError::invalid("image crop row overflowed"))?;
        rgba.extend_from_slice(image.rgba.get(start..end).ok_or_else(|| DltError::invalid("image crop exceeds decoded pixels"))?);
    }
    image.width = crop.width;
    image.height = crop.height;
    image.rgba = rgba;
    Ok(image)
}

struct BmpHeader { width: u32, height: u32, top_down: bool, bpp: u16, compression: u32, offset: usize, header_size: usize, colors_used: u32, masks: Option<[u32; 4]> }

fn le32(b: &[u8], at: usize) -> DltResult<u32> { b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).ok_or_else(|| DltError::invalid("Truncated BMP")) }
fn le16(b: &[u8], at: usize) -> DltResult<u16> { b.get(at..at + 2).map(|s| u16::from_le_bytes(s.try_into().unwrap())).ok_or_else(|| DltError::invalid("Truncated BMP")) }

fn bmp_header(b: &[u8]) -> DltResult<BmpHeader> {
    let offset = le32(b, 10)? as usize;
    let header_size = le32(b, 14)? as usize;
    let (width, height, bpp, compression, colors_used, top_down) = if header_size == 12 {
        (le16(b, 18)? as u32, le16(b, 20)? as u32, le16(b, 24)?, 0, 0, false)
    } else if header_size >= 40 {
        let w = le32(b, 18)? as i32;
        let h = le32(b, 22)? as i32;
        if w <= 0 || h == 0 { return Err(DltError::invalid("BMP has an invalid size")); }
        (w as u32, h.unsigned_abs(), le16(b, 28)?, le32(b, 30)?, le32(b, 46)?, h < 0)
    } else { return Err(DltError::unsupported(format!("BMP header size {header_size} is not supported"))); };
    let masks = if compression == 3 || compression == 6 {
        if header_size >= 52 { Some([le32(b, 54)?, le32(b, 58)?, le32(b, 62)?, if header_size >= 56 { le32(b, 66)? } else { 0 }]) }
        else { Some([le32(b, 14 + header_size)?, le32(b, 18 + header_size)?, le32(b, 22 + header_size)?, 0]) }
    } else { None };
    Ok(BmpHeader { width, height, top_down, bpp, compression, offset, header_size, colors_used, masks })
}

fn decode_bmp(b: &[u8]) -> DltResult<Decoded> {
    let h = bmp_header(b)?;
    if u64::from(h.width) * u64::from(h.height) > MAX_PIXELS { return Err(DltError::invalid("BMP is too large")); }
    let palette_entry = if h.header_size == 12 { 3 } else { 4 };
    let palette_at = 14 + h.header_size + if h.compression == 3 && h.header_size == 40 { 12 } else { 0 };
    let palette: Vec<[u8; 3]> = if h.bpp <= 8 {
        let count = if h.colors_used == 0 { 1usize << h.bpp } else { h.colors_used as usize };
        (0..count).map(|k| {
            let at = palette_at + k * palette_entry;
            b.get(at..at + 3).map(|p| [p[2], p[1], p[0]]).unwrap_or([0, 0, 0])
        }).collect()
    } else { vec![] };
    let (w, ht) = (h.width as usize, h.height as usize);
    let mut indices: Option<Vec<u8>> = None;
    if h.compression == 1 || h.compression == 2 {
        indices = Some(decode_rle(&b[h.offset.min(b.len())..], w, ht, h.compression == 2)?);
    } else if !matches!(h.compression, 0 | 3 | 6) {
        return Err(DltError::unsupported(format!("BMP compression {} is not supported", h.compression)));
    }
    let stride = ((w * h.bpp as usize + 31) / 32) * 4;
    let mut rgba = vec![0u8; w * ht * 4];
    for row in 0..ht {
        let src_row = if h.top_down { row } else { ht - 1 - row };
        for x in 0..w {
            let px: [u8; 4] = if let Some(ix) = &indices {
                let c = palette.get(ix[src_row * w + x] as usize).copied().unwrap_or([0, 0, 0]);
                [c[0], c[1], c[2], 255]
            } else {
                let base = h.offset + src_row * stride;
                match h.bpp {
                    1 | 4 | 8 => {
                        let bit = x * h.bpp as usize;
                        let byte = *b.get(base + bit / 8).ok_or_else(|| DltError::invalid("Truncated BMP pixels"))?;
                        let shift = 8 - h.bpp as usize - (bit % 8);
                        let ix = (byte >> shift) & ((1u16 << h.bpp) - 1) as u8;
                        let c = palette.get(ix as usize).copied().unwrap_or([0, 0, 0]);
                        [c[0], c[1], c[2], 255]
                    }
                    24 => {
                        let p = b.get(base + x * 3..base + x * 3 + 3).ok_or_else(|| DltError::invalid("Truncated BMP pixels"))?;
                        [p[2], p[1], p[0], 255]
                    }
                    16 | 32 => {
                        let bytes = h.bpp as usize / 8;
                        let p = b.get(base + x * bytes..base + x * bytes + bytes).ok_or_else(|| DltError::invalid("Truncated BMP pixels"))?;
                        let v = if bytes == 2 { u32::from(u16::from_le_bytes([p[0], p[1]])) } else { u32::from_le_bytes([p[0], p[1], p[2], p[3]]) };
                        let masks = h.masks.unwrap_or(if bytes == 2 { [0x7C00, 0x03E0, 0x001F, 0] } else { [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0] });
                        let a = if masks[3] != 0 { channel(v, masks[3]) } else { 255 };
                        [channel(v, masks[0]), channel(v, masks[1]), channel(v, masks[2]), a]
                    }
                    other => return Err(DltError::unsupported(format!("BMP with {other} bits per pixel is not supported"))),
                }
            };
            let at = (row * w + x) * 4;
            rgba[at..at + 4].copy_from_slice(&px);
        }
    }
    Ok(Decoded { width: h.width, height: h.height, rgba, warnings: vec![] })
}

fn channel(v: u32, mask: u32) -> u8 {
    if mask == 0 { return 0; }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).count_ones();
    let raw = (v & mask) >> shift;
    let max = (1u64 << bits) - 1;
    ((u64::from(raw) * 255 + max / 2) / max) as u8
}

fn decode_rle(data: &[u8], w: usize, h: usize, four: bool) -> DltResult<Vec<u8>> {
    let mut out = vec![0u8; w * h];
    let (mut x, mut y) = (0usize, 0usize);
    let mut i = 0;
    let mut put = |x: &mut usize, y: usize, v: u8| { if *x < w && y < h { out[y * w + *x] = v; } *x += 1; };
    while i + 1 < data.len() {
        let (n, c) = (data[i] as usize, data[i + 1]);
        i += 2;
        if n > 0 {
            for k in 0..n { let v = if four { if k % 2 == 0 { c >> 4 } else { c & 0x0F } } else { c }; put(&mut x, y, v); }
        } else {
            match c {
                0 => { x = 0; y += 1; }
                1 => break,
                2 => { if i + 1 >= data.len() { break; } x += data[i] as usize; y += data[i + 1] as usize; i += 2; }
                count => {
                    let count = count as usize;
                    let bytes = if four { count.div_ceil(2) } else { count };
                    for k in 0..count {
                        let byte = *data.get(i + if four { k / 2 } else { k }).unwrap_or(&0);
                        let v = if four { if k % 2 == 0 { byte >> 4 } else { byte & 0x0F } } else { byte };
                        put(&mut x, y, v);
                    }
                    i += bytes + (bytes % 2);
                }
            }
        }
    }
    // Rows are in file order (the caller maps bottom-up/top-down like uncompressed data).
    Ok(out)
}

/// Standard base64 (RFC 4648) without line breaks, for data URIs. The implementation is `designllm_contracts::base64`.
pub fn base64(data: &[u8]) -> String {
    designllm_contracts::base64::encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn colors_and_base64() {
        assert_eq!(parse_color("#20252D"), Ok(Some(Rgba { r: 0x20, g: 0x25, b: 0x2D, a: 255 })));
        assert_eq!(parse_color("#fff"), Ok(Some(Rgba { r: 255, g: 255, b: 255, a: 255 })));
        assert_eq!(parse_color("none"), Ok(None));
        assert!(parse_color("#12").is_err());
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }

    #[test]
    fn bmp_24bit_bottom_up() {
        // 2x2 BI_RGB 24-bit: bottom row blue, green; top row red, white.
        let mut b = vec![];
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&(70u32).to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&(54u32).to_le_bytes());
        b.extend_from_slice(&(40u32).to_le_bytes());
        b.extend_from_slice(&(2i32).to_le_bytes());
        b.extend_from_slice(&(2i32).to_le_bytes());
        b.extend_from_slice(&(1u16).to_le_bytes());
        b.extend_from_slice(&(24u16).to_le_bytes());
        b.extend_from_slice(&[0; 24]);
        b.extend_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0]);
        b.extend_from_slice(&[0, 0, 255, 255, 255, 255, 0, 0]);
        let d = decode(&b).unwrap();
        assert_eq!((d.width, d.height), (2, 2));
        assert_eq!(&d.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&d.rgba[4..8], &[255, 255, 255, 255]);
        assert_eq!(&d.rgba[8..12], &[0, 0, 255, 255]);
        assert_eq!(&d.rgba[12..16], &[0, 255, 0, 255]);
    }

    fn gray_png_1x1() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0]).unwrap();
        }
        bytes
    }

    fn png_bytes(width: u32, height: u32, color: png::ColorType, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(color);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(pixels).unwrap();
        }
        bytes
    }

    fn rgba_apng_with_a_subframe_after_the_default_frame() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(2, 0).unwrap();
            encoder.validate_sequence(true);
            let mut writer = encoder.write_header().unwrap();
            // PNG requires an IDAT-backed default frame to cover the whole IHDR canvas.
            writer.write_image_data(&[10, 20, 30, 40, 50, 60, 70, 80]).unwrap();
            writer.set_frame_dimension(1, 1).unwrap();
            writer.set_frame_position(1, 0).unwrap();
            writer.write_image_data(&[90, 100, 110, 120]).unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    fn jpeg_with_sof(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0, 0, 17, 8];
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
        bytes.extend_from_slice(&[0xFF, 0xDA, 0, 12, 3, 1, 0, 2, 0, 3, 0, 0, 63, 0]);
        bytes
    }

    fn jpeg_with_exif_orientation(orientation: u16) -> Vec<u8> {
        let mut tiff = b"II\x2A\x00\x08\x00\x00\x00\x01\x00\x12\x01\x03\x00\x01\x00\x00\x00".to_vec();
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&tiff);
        let length = u16::try_from(payload.len() + 2).unwrap();
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE1];
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&jpeg_with_sof(4, 2)[2..]);
        bytes
    }

    fn gif_with_transparent_offset_frame() -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 255, 0, 0]);
        bytes.extend_from_slice(&[0x21, 0xF9, 4, 1, 0, 0, 0, 0]);
        bytes.push(0x2C);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[2, 2, 0x44, 0x0A, 0, 0x3B]);
        bytes
    }

    fn lossless_webp(width: u32, height: u32, data: &[u8], color: image::ExtendedColorType) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)
            .encode(data, width, height, color)
            .expect("small WebP fixture encodes");
        bytes
    }

    fn webp_file(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
        for (tag, payload) in chunks {
            bytes.extend_from_slice(*tag);
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(payload);
            if payload.len() % 2 != 0 { bytes.push(0); }
        }
        let riff_size = u32::try_from(bytes.len() - 8).expect("small test WebP size");
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        bytes
    }

    fn webp_vp8x(width: u32, height: u32, flags: u8) -> [u8; 10] {
        let width_minus_one = width - 1;
        let height_minus_one = height - 1;
        [
            flags, 0, 0, 0,
            width_minus_one as u8, (width_minus_one >> 8) as u8, (width_minus_one >> 16) as u8,
            height_minus_one as u8, (height_minus_one >> 8) as u8, (height_minus_one >> 16) as u8,
        ]
    }

    fn webp_vp8l_payload(bytes: &[u8]) -> Vec<u8> {
        assert_eq!(&bytes[12..16], b"VP8L");
        let len = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        bytes[20..20 + len].to_vec()
    }

    #[test]
    fn oversized_png_dimensions_are_rejected_before_frame_allocation() {
        let mut bytes = gray_png_1x1();
        bytes[16..20].copy_from_slice(&((MAX_PIXELS as u32) + 1).to_be_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_be_bytes());

        assert!(matches!(decode_png_with(&bytes, true), Err(png::DecodingError::LimitsExceeded)));
        let valid = decode_png(&gray_png_1x1()).expect("a valid 1x1 PNG still decodes");
        assert_eq!((valid.width, valid.height, valid.rgba.as_slice()), (1, 1, &[0, 0, 0, 255][..]));
    }

    #[test]
    fn rgba_png_reuses_the_decoder_buffer_without_changing_alpha_or_rows() {
        let pixels = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160];
        let decoded = decode_png(&png_bytes(2, 2, png::ColorType::Rgba, &pixels)).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.rgba.as_slice(), &pixels);

        let rgb = [1, 2, 3, 4, 5, 6];
        let decoded = decode_png(&png_bytes(1, 2, png::ColorType::Rgb, &rgb)).unwrap();
        assert_eq!(decoded.rgba.as_slice(), &[1, 2, 3, 255, 4, 5, 6, 255], "non-RGBA rows still expand with opaque alpha");
    }

    #[test]
    fn rgba_apng_keeps_the_full_default_frame_when_later_frames_are_subframes() {
        let decoded = decode_png(&rgba_apng_with_a_subframe_after_the_default_frame()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.rgba.as_slice(), &[10, 20, 30, 40, 50, 60, 70, 80]);
    }

    #[test]
    fn oversized_jpeg_dimensions_are_checked_from_the_decoder_header() {
        // Fits the decoder's per-axis limits but exceeds the total pixel cap.
        let bytes = jpeg_with_sof(2049, 32768);
        let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
            &bytes,
            zune_jpeg::zune_core::options::DecoderOptions::default()
                .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGBA)
                .set_max_width(1 << 15)
                .set_max_height(1 << 15),
        );
        decoder.decode_headers().expect("the JPEG frame and scan headers parse");
        assert_eq!(decoder.dimensions(), Some((2049, 32768)));
        let error = decode_jpeg(&bytes).err().expect("the pixel cap is applied before decoding");
        assert!(error.message.contains("64 megapixel limit"), "{error:?}");
    }

    #[test]
    fn jpeg_exif_orientation_has_one_shared_fail_closed_parse() {
        assert_eq!(jpeg_exif_orientation(&jpeg_with_sof(4, 2)).unwrap(), None);
        assert_eq!(jpeg_exif_orientation(&jpeg_with_exif_orientation(1)).unwrap(), None);
        assert_eq!(jpeg_exif_orientation(&jpeg_with_exif_orientation(6)).unwrap(), Some(6));
        assert!(jpeg_exif_orientation(&jpeg_with_exif_orientation(9)).is_err(), "invalid orientation values are malformed metadata");

        let mut malformed_tiff = jpeg_with_exif_orientation(6);
        malformed_tiff[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(jpeg_exif_orientation(&malformed_tiff).is_err(), "out-of-range IFD offsets are rejected");

        let mut conflicting = jpeg_with_exif_orientation(6);
        let second = jpeg_with_exif_orientation(3);
        let second_segment = &second[2..2 + 2 + u16::from_be_bytes([second[4], second[5]]) as usize];
        let insert_at = conflicting.len() - jpeg_with_sof(4, 2).len() + 2;
        conflicting.splice(insert_at..insert_at, second_segment.iter().copied());
        assert!(jpeg_exif_orientation(&conflicting).is_err(), "conflicting EXIF orientations are ambiguous");
    }

    #[test]
    fn crop_decoded_image_copies_only_the_selected_rows_and_preserves_alpha() {
        let image = Decoded {
            width: 2,
            height: 2,
            rgba: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            warnings: vec!["kept warning".into()],
        };
        let cropped = crop_decoded_image(image, designllm_contracts::PixelRect { x: 1, y: 0, width: 1, height: 2 }).unwrap();
        assert_eq!((cropped.width, cropped.height), (1, 2));
        assert_eq!(cropped.rgba.as_slice(), &[5, 6, 7, 8, 13, 14, 15, 16]);
        assert_eq!(cropped.warnings[0], "kept warning");
    }

    #[test]
    fn crop_decoded_image_rejects_empty_out_of_bounds_and_inconsistent_buffers() {
        for crop in [
            designllm_contracts::PixelRect { x: 0, y: 0, width: 0, height: 1 },
            designllm_contracts::PixelRect { x: 2, y: 0, width: 1, height: 1 },
            designllm_contracts::PixelRect { x: 0, y: u32::MAX, width: 1, height: 1 },
        ] {
            assert!(crop_decoded_image(Decoded { width: 2, height: 2, rgba: vec![0; 16], warnings: vec![] }, crop).is_err());
        }
        assert!(crop_decoded_image(
            Decoded { width: 2, height: 2, rgba: vec![0; 15], warnings: vec![] },
            designllm_contracts::PixelRect { x: 0, y: 0, width: 1, height: 1 },
        ).is_err());
    }

    #[test]
    fn gif_decodes_the_first_logical_canvas_frame_with_offset_and_transparency() {
        let decoded = decode(&gif_with_transparent_offset_frame()).expect("valid offset GIF decodes");
        assert_eq!((decoded.width, decoded.height), (3, 1));
        assert_eq!(decoded.rgba.as_slice(), &[0, 0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 255]);
    }

    #[test]
    fn gif_rejects_truncated_and_oversized_headers_before_image_allocation() {
        assert!(decode(b"GIF89a").is_err());

        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&8193u16.to_le_bytes());
        bytes.extend_from_slice(&8192u16.to_le_bytes());
        assert!(decode(&bytes).err().is_some_and(|error| error.message.contains("64 megapixel limit")));
    }

    #[test]
    fn static_webp_decodes_alpha_and_expands_rgb_to_opaque_rgba() {
        let rgba = [1, 2, 3, 0, 40, 50, 60, 127];
        let bytes = lossless_webp(2, 1, &rgba, image::ExtendedColorType::Rgba8);
        assert_eq!(pixel_size(&bytes), Some((2, 1)));
        let decoded = decode(&bytes).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.rgba.as_slice(), &rgba);

        let rgb = [7, 8, 9, 10, 11, 12];
        let decoded = decode(&lossless_webp(2, 1, &rgb, image::ExtendedColorType::Rgb8)).unwrap();
        assert_eq!(decoded.rgba.as_slice(), &[7, 8, 9, 255, 10, 11, 12, 255]);
    }

    #[test]
    fn webp_rejects_malformed_canvas_and_frame_dimensions_before_decode() {
        let image = webp_vp8l_payload(&lossless_webp(1, 1, &[1, 2, 3, 255], image::ExtendedColorType::Rgba8));
        let oversized_canvas = webp_file(&[
            (b"VP8X", &webp_vp8x(8193, 8192, 0)),
            (b"VP8L", &image),
        ]);
        assert!(decode(&oversized_canvas).err().is_some_and(|error| error.message.contains("canvas exceeds")));

        let mismatched_canvas = webp_file(&[(b"VP8X", &webp_vp8x(2, 1, 0)), (b"VP8L", &image)]);
        assert!(decode(&mismatched_canvas).err().is_some_and(|error| error.message.contains("do not match the canvas")));

        let mut oversized_vp8 = vec![0, 0, 0, 0x9d, 0x01, 0x2a];
        oversized_vp8.extend_from_slice(&8193u16.to_le_bytes());
        oversized_vp8.extend_from_slice(&8192u16.to_le_bytes());
        let oversized_frame = webp_file(&[(b"VP8 ", &oversized_vp8)]);
        assert!(decode(&oversized_frame).err().is_some_and(|error| error.message.contains("frame exceeds")));

        let mut truncated = lossless_webp(1, 1, &[1, 2, 3, 255], image::ExtendedColorType::Rgba8);
        truncated.pop();
        assert!(decode(&truncated).is_err());
    }

    #[test]
    fn extended_webp_uses_only_chunks_inside_its_declared_riff_extent() {
        let payload = webp_vp8l_payload(&lossless_webp(1, 1, &[1, 2, 3, 255], image::ExtendedColorType::Rgba8));
        let vp8x = webp_vp8x(1, 1, 0);
        let valid = webp_file(&[(b"VP8X", &vp8x), (b"VP8L", &payload)]);
        let decoded = decode(&valid).unwrap();
        assert_eq!((decoded.width, decoded.height), (1, 1));
        assert_eq!(pixel_size(&valid), Some((1, 1)));

        let mut trailing_alpha_chunk = valid;
        trailing_alpha_chunk.extend_from_slice(b"ALPH");
        trailing_alpha_chunk.extend_from_slice(&2u32.to_le_bytes());
        trailing_alpha_chunk.extend_from_slice(&[0, 0]);
        assert!(decode(&trailing_alpha_chunk).is_err(), "out-of-container chunks are rejected before image-webp can scan them");
        assert_eq!(pixel_size(&trailing_alpha_chunk), None);
    }

    #[test]
    fn animated_webp_is_rejected_instead_of_rendering_a_different_static_frame() {
        let vp8x = webp_vp8x(1, 1, 0b10);
        let animation = [0u8; 6];
        let mut frame = vec![0; 16];
        frame.extend_from_slice(b"VP8L");
        frame.extend_from_slice(&5u32.to_le_bytes());
        frame.extend_from_slice(&[0x2f, 0, 0, 0, 0]);
        if (frame.len() - 16) % 2 != 0 { frame.push(0); }
        let animated = webp_file(&[(b"VP8X", &vp8x), (b"ANIM", &animation), (b"ANMF", &frame)]);
        let error = decode(&animated).err().expect("animated WebP is explicitly unsupported");
        assert_eq!(error.code, designllm_contracts::ErrorCode::UnsupportedOperation);
        assert!(error.message.contains("Animated WebP"));
    }
}

