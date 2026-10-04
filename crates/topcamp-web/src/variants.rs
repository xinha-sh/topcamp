//! Synchronous image variants (UI-07.2): account-logo PNGs and the
//! avatar WebP, processed at request time with the `image` crate
//! (PNG/JPEG decode only — anything else is not variable, like
//! upstream's `logo.variable?` gate).
//!
//! Specs mirror upstream exactly: logos are `resize_to_limit`
//! 192 (`?size=small`) or 512 PNG; avatars are `resize_to_limit`
//! 512 WebP (`:square`). Digests are hex sha256 of the transform
//! spec (same convention as the workflow thumbnails), so variant
//! rows + store keys are stable and self-healing: a missing variant
//! is simply re-derived from the original.

use sha2::{Digest, Sha256};

/// `resize_to_limit:192x192,png` (`?size=small`).
pub(crate) fn logo_small_digest() -> String {
    spec_digest("resize_to_limit:192x192,png")
}

/// `resize_to_limit:512x512,png` (default logo size).
pub(crate) fn logo_large_digest() -> String {
    spec_digest("resize_to_limit:512x512,png")
}

/// `resize_to_limit:512x512,webp` (avatar `:square`).
pub(crate) fn avatar_digest() -> String {
    spec_digest("resize_to_limit:512x512,webp")
}

fn spec_digest(spec: &str) -> String {
    format!("{:x}", Sha256::digest(spec))
}

/// `resize_to_limit`: shrink to fit, never enlarge (`thumbnail`
/// alone upscales small images; vips does not).
fn fit(img: &image::DynamicImage, max: u32) -> image::DynamicImage {
    if img.width() > max || img.height() > max {
        img.thumbnail(max, max)
    } else {
        img.clone()
    }
}

/// Decode raster bytes and fit within `max`×`max` (no upscale),
/// encoding PNG. `None` when the bytes are not a decodable raster.
pub(crate) fn resize_png(original: &[u8], max: u32) -> Option<Vec<u8>> {
    let img = image::load_from_memory(original).ok()?;
    let resized = fit(&img, max);
    let mut encoded = Vec::new();
    resized
        .write_to(
            &mut std::io::Cursor::new(&mut encoded),
            image::ImageFormat::Png,
        )
        .ok()?;
    Some(encoded)
}

/// Decode raster bytes and fit within 512×512 (no upscale), encoding
/// WebP. `None` when the bytes are not a decodable raster.
pub(crate) fn square_webp(original: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(original).ok()?;
    let resized = fit(&img, 512);
    let mut encoded = Vec::new();
    resized
        .write_to(
            &mut std::io::Cursor::new(&mut encoded),
            image::ImageFormat::WebP,
        )
        .ok()?;
    Some(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x2 red PNG, built without an encoder (hand-rolled IHDR).
    fn tiny_png() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(4, 2, image::Rgb([255, 0, 0]));
        let mut encoded = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut encoded),
                image::ImageFormat::Png,
            )
            .unwrap();
        encoded
    }

    #[test]
    fn digests_are_stable_and_distinct() {
        assert_eq!(
            logo_small_digest(),
            spec_digest("resize_to_limit:192x192,png")
        );
        assert_eq!(
            logo_large_digest(),
            spec_digest("resize_to_limit:512x512,png")
        );
        assert_eq!(avatar_digest(), spec_digest("resize_to_limit:512x512,webp"));
        assert_ne!(logo_small_digest(), logo_large_digest());
    }

    #[test]
    fn png_variant_round_trips() {
        let out = resize_png(&tiny_png(), 192).expect("png processes");
        assert_eq!(&out[1..4], b"PNG");
        let back = image::load_from_memory(&out).unwrap();
        assert_eq!((back.width(), back.height()), (4, 2));
    }

    #[test]
    fn webp_variant_round_trips() {
        let out = square_webp(&tiny_png()).expect("webp processes");
        assert_eq!(&out[0..4], b"RIFF");
        assert_eq!(&out[8..12], b"WEBP");
    }

    #[test]
    fn garbage_is_not_variable() {
        assert!(resize_png(b"not an image", 192).is_none());
        assert!(square_webp(b"not an image").is_none());
    }
}
