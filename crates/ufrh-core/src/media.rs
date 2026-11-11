//! Images attached to prompts: format sniffing, downscaling and base64 encoding.
//!
//! Only PNG is decoded (the format of every clipboard screenshot); JPEG/GIF/WebP pass through
//! unchanged, so the `image` crate stays at its `png` feature (already in the GUI's tree).

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::path::Path;
use ufrh_types::Image;

/// Longest edge sent to the model. Anthropic downsizes anything larger anyway (≈1.15 MP), and
/// OpenAI's high-detail mode tiles at 768 px on the short side, so bigger only costs bytes.
pub const MAX_EDGE: u32 = 1568;
/// Per-image limit of the strictest provider (Anthropic: 5 MB).
pub const MAX_BYTES: usize = 5 * 1024 * 1024;

const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Media type from the file's magic bytes.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Whether the path has an image extension (cheap pre-check before reading the file).
pub fn is_image_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// Encoded image bytes (PNG/JPEG/GIF/WebP) → attachment. PNGs larger than [`MAX_EDGE`] are
/// downscaled and re-encoded.
pub fn image_from_bytes(bytes: Vec<u8>) -> Result<Image> {
    let Some(mt) = sniff(&bytes) else {
        bail!("not a PNG, JPEG, GIF or WebP image");
    };
    let bytes = if mt == "image/png" {
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .context("invalid PNG")?;
        if img.width().max(img.height()) > MAX_EDGE {
            encode_png(img)?
        } else {
            bytes
        }
    } else {
        bytes
    };
    finish(mt, &bytes)
}

/// Raw RGBA pixels (e.g. from the X11 clipboard) → PNG attachment.
pub fn image_from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<Image> {
    let buf = image::RgbaImage::from_raw(width, height, rgba)
        .context("pixel buffer does not match the image size")?;
    finish(
        "image/png",
        &encode_png(image::DynamicImage::ImageRgba8(buf))?,
    )
}

pub fn load_image_file(path: &Path) -> Result<Image> {
    let meta = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
    if meta.len() > 64 * 1024 * 1024 {
        bail!("{}: file too large", path.display());
    }
    let bytes = std::fs::read(path).with_context(|| format!("{}", path.display()))?;
    image_from_bytes(bytes).with_context(|| format!("{}", path.display()))
}

/// Width and height read from the PNG header, without decoding the image.
pub fn dimensions(img: &Image) -> Option<(u32, u32)> {
    if img.media_type != "image/png" || img.data.len() < 32 {
        return None;
    }
    let head = STANDARD.decode(&img.data[..32]).ok()?; // 24 bytes: signature + IHDR start
    let be = |i: usize| u32::from_be_bytes(head[i..i + 4].try_into().unwrap());
    Some((be(16), be(20)))
}

/// Small RGBA preview (longest edge ≤ `max`) for the UI. PNG only.
pub fn thumbnail(img: &Image, max: u32) -> Option<(u32, u32, Vec<u8>)> {
    if img.media_type != "image/png" {
        return None;
    }
    let bytes = STANDARD.decode(&img.data).ok()?;
    let full = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?;
    let t = full.thumbnail(max, max).to_rgba8();
    Some((t.width(), t.height(), t.into_raw()))
}

fn encode_png(img: image::DynamicImage) -> Result<Vec<u8>> {
    let img = if img.width().max(img.height()) > MAX_EDGE {
        img.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    // Screenshots are opaque: dropping the alpha channel makes the PNG ~25% smaller.
    let img = if img.color().has_alpha() && img.to_rgba8().pixels().all(|p| p.0[3] == 255) {
        image::DynamicImage::ImageRgb8(img.to_rgb8())
    } else {
        img
    };
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

fn finish(media_type: &str, bytes: &[u8]) -> Result<Image> {
    if bytes.len() > MAX_BYTES {
        bail!(
            "image too large ({:.1} MB; the limit is {} MB)",
            bytes.len() as f64 / 1048576.0,
            MAX_BYTES / 1048576
        );
    }
    Ok(Image {
        media_type: media_type.to_owned(),
        data: STANDARD.encode(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn small_png_passes_through() {
        let bytes = png(40, 30);
        let img = image_from_bytes(bytes.clone()).unwrap();
        assert_eq!(img.media_type, "image/png");
        assert_eq!(STANDARD.decode(&img.data).unwrap(), bytes);
        assert_eq!(dimensions(&img), Some((40, 30)));
        let (w, h, px) = thumbnail(&img, 8).unwrap();
        assert_eq!((w, h), (8, 6));
        assert_eq!(px.len(), 8 * 6 * 4);
    }

    #[test]
    fn large_images_are_downscaled() {
        let img = image_from_bytes(png(3136, 1000)).unwrap();
        assert_eq!(dimensions(&img), Some((MAX_EDGE, 500)));
        let rgba = vec![255u8; 2000 * 100 * 4];
        let img = image_from_rgba(2000, 100, rgba).unwrap();
        assert_eq!(dimensions(&img).unwrap().0, MAX_EDGE);
    }

    #[test]
    fn sniffing() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"GIF89a.."), Some("image/gif"));
        assert!(image_from_bytes(b"hello".to_vec()).is_err());
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];
        assert_eq!(image_from_bytes(jpeg).unwrap().media_type, "image/jpeg");
        assert!(is_image_path(Path::new("a/Shot.PNG")));
        assert!(!is_image_path(Path::new("a/notes.txt")));
    }
}
