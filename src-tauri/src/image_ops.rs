//! Decode, thumbnail, and palette sampling.

use image::{DynamicImage, GenericImageView, ImageReader};

use crate::color::{srgb_to_oklab, Oklab};
use crate::error::{Error, Result};

/// Long edge of generated thumbnails, in pixels.
pub const THUMB_LONG_EDGE: u32 = 512;

/// WebP quality for thumbnails. 82 is around the knee of the size/artefact
/// curve for UI screenshots, which are the harshest case in a reference
/// library -- flat colour next to small text is where low-quality WebP rings.
const THUMB_QUALITY: f32 = 82.0;

/// Longest edge used for palette clustering. k-means cost scales with pixel
/// count and a palette is a global property, so there is nothing to gain from
/// clustering full resolution.
const PALETTE_SAMPLE_EDGE: u32 = 64;

/// Alpha below this is treated as transparent and excluded from the palette.
/// Without this, a logo on transparency yields a palette dominated by whatever
/// the decoder left in the RGB channels of fully-transparent pixels -- often
/// black, which is a colour the user never sees.
const ALPHA_CUTOFF: u8 = 16;

pub fn decode(path: &std::path::Path, bytes: &[u8]) -> Result<DynamicImage> {
    // Sniff by content rather than extension: browsers hand over .png files
    // that are really JPEG or AVIF often enough to matter.
    let reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::io(path, e))?;

    // The extension recorded in the DB comes from `format_of` (also sniffed),
    // not the original filename, so blob paths never lie about their contents.
    reader.decode().map_err(|source| Error::Decode {
        path: path.to_path_buf(),
        source,
    })
}

/// Canonical extension and MIME type for a decoded image's real format.
pub fn format_of(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    let format = image::guess_format(bytes).ok()?;
    Some(match format {
        image::ImageFormat::Png => ("png", "image/png"),
        image::ImageFormat::Jpeg => ("jpg", "image/jpeg"),
        image::ImageFormat::WebP => ("webp", "image/webp"),
        image::ImageFormat::Gif => ("gif", "image/gif"),
        image::ImageFormat::Avif => ("avif", "image/avif"),
        image::ImageFormat::Bmp => ("bmp", "image/bmp"),
        image::ImageFormat::Tiff => ("tiff", "image/tiff"),
        image::ImageFormat::Ico => ("ico", "image/x-icon"),
        _ => return None,
    })
}

/// MIME type for a container extension.
///
/// The extension-driven counterpart to [`format_of`], for the case where the
/// bytes are not in hand: a linked image is described by its URL, and the only
/// thing downloaded is a thumbnail that may be in a different format entirely.
pub fn mime_for_extension(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "ico" => "image/x-icon",
        // Including "jpg"/"jpeg". Guessing jpeg for an unknown extension beats
        // inventing a type: it is what CDNs serve when they say nothing.
        _ => "image/jpeg",
    }
}

/// Scales so the long edge is at most `long_edge`, preserving aspect ratio.
/// Images already smaller are returned untouched rather than upscaled.
pub fn thumbnail(image: &DynamicImage, long_edge: u32) -> DynamicImage {
    let (w, h) = image.dimensions();
    if w.max(h) <= long_edge {
        return image.clone();
    }
    // Lanczos3 over the cheaper filters: reference thumbnails are looked at,
    // not just indexed, and Triangle visibly softens small text.
    image.resize(long_edge, long_edge, image::imageops::FilterType::Lanczos3)
}

pub fn encode_webp(image: &DynamicImage) -> Result<Vec<u8>> {
    // libwebp only accepts RGB8/RGBA8. Preserve alpha when present; dropping it
    // would composite logos onto black.
    let encoded = if image.color().has_alpha() {
        let rgba = image.to_rgba8();
        let (w, h) = rgba.dimensions();
        webp::Encoder::from_rgba(rgba.as_raw(), w, h).encode(THUMB_QUALITY)
    } else {
        let rgb = image.to_rgb8();
        let (w, h) = rgb.dimensions();
        webp::Encoder::from_rgb(rgb.as_raw(), w, h).encode(THUMB_QUALITY)
    };
    Ok(encoded.to_vec())
}

/// Downsamples and converts to OkLab for clustering, dropping transparent pixels.
pub fn palette_samples(image: &DynamicImage) -> Vec<Oklab> {
    let small = image.resize(
        PALETTE_SAMPLE_EDGE,
        PALETTE_SAMPLE_EDGE,
        // Nearest would alias a dithered gradient into a few hard bands and
        // invent colours; Triangle averages, which is what a palette wants.
        image::imageops::FilterType::Triangle,
    );
    small
        .to_rgba8()
        .pixels()
        .filter(|p| p.0[3] >= ALPHA_CUTOFF)
        .map(|p| srgb_to_oklab(p.0[0], p.0[1], p.0[2]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(w, h, Rgba(rgba)))
    }

    fn png_bytes(image: &DynamicImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    #[test]
    fn thumbnail_caps_long_edge_and_keeps_aspect() {
        let wide = solid(2000, 1000, [255, 0, 0, 255]);
        let t = thumbnail(&wide, THUMB_LONG_EDGE);
        assert_eq!(t.dimensions(), (512, 256));

        let tall = solid(1000, 2000, [255, 0, 0, 255]);
        let t = thumbnail(&tall, THUMB_LONG_EDGE);
        assert_eq!(t.dimensions(), (256, 512));
    }

    #[test]
    fn small_images_are_not_upscaled() {
        let small = solid(64, 48, [0, 0, 255, 255]);
        assert_eq!(thumbnail(&small, THUMB_LONG_EDGE).dimensions(), (64, 48));
    }

    #[test]
    fn decode_round_trips_a_png() {
        let original = solid(8, 8, [10, 20, 30, 255]);
        let bytes = png_bytes(&original);
        let decoded = decode(std::path::Path::new("t.png"), &bytes).expect("decode");
        assert_eq!(decoded.dimensions(), (8, 8));
    }

    #[test]
    fn decode_reports_the_offending_path() {
        let err = decode(std::path::Path::new("broken.png"), b"not an image").unwrap_err();
        assert!(
            err.to_string().contains("broken.png"),
            "error should name the file, got: {err}"
        );
    }

    #[test]
    fn format_is_sniffed_from_content_not_extension() {
        let bytes = png_bytes(&solid(4, 4, [1, 2, 3, 255]));
        assert_eq!(format_of(&bytes), Some(("png", "image/png")));
        assert_eq!(format_of(b"not an image"), None);
    }

    #[test]
    fn webp_encoding_produces_a_decodable_riff() {
        let bytes = encode_webp(&solid(32, 32, [200, 100, 50, 255])).expect("encode");
        assert!(bytes.len() > 16);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
        // Must survive a real decode, not just look like a header.
        assert_eq!(
            image::load_from_memory(&bytes)
                .expect("decode webp")
                .dimensions(),
            (32, 32)
        );
    }

    #[test]
    fn webp_encoding_preserves_alpha_channel() {
        let bytes = encode_webp(&solid(16, 16, [255, 0, 0, 0])).expect("encode");
        let decoded = image::load_from_memory(&bytes).expect("decode");
        assert!(
            decoded.color().has_alpha(),
            "alpha was dropped during encoding"
        );
    }

    #[test]
    fn transparent_pixels_are_excluded_from_palette_samples() {
        let transparent = solid(32, 32, [0, 0, 0, 0]);
        assert!(
            palette_samples(&transparent).is_empty(),
            "fully transparent image should contribute no samples"
        );

        let opaque = solid(32, 32, [120, 60, 200, 255]);
        assert!(!palette_samples(&opaque).is_empty());
    }

    #[test]
    fn palette_samples_are_downsampled() {
        let big = solid(1024, 1024, [5, 5, 5, 255]);
        let samples = palette_samples(&big);
        assert!(
            samples.len() <= (PALETTE_SAMPLE_EDGE * PALETTE_SAMPLE_EDGE) as usize,
            "expected at most {}^2 samples, got {}",
            PALETTE_SAMPLE_EDGE,
            samples.len()
        );
    }
}
