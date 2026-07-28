//! Palette extraction in OkLab.
//!
//! Clustering happens in OkLab rather than sRGB because sRGB distance does not
//! track perceived difference -- two greens a fixed sRGB distance apart look far
//! closer than two blues the same distance apart, so k-means in sRGB produces
//! clusters that disagree with what a designer would call "the same colour".
//! OkLab is near-perceptually-uniform, which makes plain Euclidean distance a
//! usable stand-in for perceptual difference.

use serde::{Deserialize, Serialize};

/// A single palette entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Swatch {
    pub l: f32,
    pub a: f32,
    pub b: f32,
    /// `#rrggbb`, the cluster centroid converted back to sRGB.
    pub hex: String,
    /// Share of sampled pixels in this cluster, 0.0..=1.0.
    pub weight: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklab {
    pub l: f32,
    pub a: f32,
    pub b: f32,
}

impl Oklab {
    fn distance_sq(self, other: Oklab) -> f32 {
        let dl = self.l - other.l;
        let da = self.a - other.a;
        let db = self.b - other.b;
        dl * dl + da * da + db * db
    }
}

fn srgb_component_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_component_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB bytes to OkLab. Matrices from Björn Ottosson's reference implementation.
pub fn srgb_to_oklab(r: u8, g: u8, b: u8) -> Oklab {
    let r = srgb_component_to_linear(f32::from(r) / 255.0);
    let g = srgb_component_to_linear(f32::from(g) / 255.0);
    let b = srgb_component_to_linear(f32::from(b) / 255.0);

    let l = 0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b;
    let m = 0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b;
    let s = 0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b;

    let l_ = l.cbrt();
    let m_ = m.cbrt();
    let s_ = s.cbrt();

    Oklab {
        l: 0.210_454_26 * l_ + 0.793_617_8 * m_ - 0.004_072_047 * s_,
        a: 1.977_998_5 * l_ - 2.428_592_2 * m_ + 0.450_593_7 * s_,
        b: 0.025_904_037 * l_ + 0.782_771_77 * m_ - 0.808_675_77 * s_,
    }
}

/// OkLab back to sRGB bytes. Out-of-gamut results are clamped, which is fine
/// here: centroids are averages of in-gamut pixels and land at most marginally
/// outside.
pub fn oklab_to_srgb(c: Oklab) -> (u8, u8, u8) {
    let l_ = c.l + 0.396_337_78 * c.a + 0.215_803_76 * c.b;
    let m_ = c.l - 0.105_561_346 * c.a - 0.063_854_17 * c.b;
    let s_ = c.l - 0.089_484_18 * c.a - 1.291_485_5 * c.b;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    let r = 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s;
    let g = -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s;
    let b = -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s;

    let to_byte = |v: f32| (linear_component_to_srgb(v).clamp(0.0, 1.0) * 255.0).round() as u8;
    (to_byte(r), to_byte(g), to_byte(b))
}

pub fn hex_of(c: Oklab) -> String {
    let (r, g, b) = oklab_to_srgb(c);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Parses `#rgb`, `#rrggbb`, or the same without the leading `#`.
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim().trim_start_matches('#');
    match s.len() {
        3 => {
            let mut it = s.chars().map(|c| c.to_digit(16).map(|d| (d * 17) as u8));
            Some((it.next()??, it.next()??, it.next()??))
        }
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some((r, g, b))
        }
        _ => None,
    }
}

/// Deterministic xorshift. k-means needs randomness for seeding, but an image
/// must always yield the same palette -- otherwise a re-index silently changes
/// every stored swatch and invalidates saved colour searches.
struct Rng(u64);

impl Rng {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        // Top 24 bits into [0,1) -- 24 is f32's mantissa width, so every
        // result is exactly representable.
        ((self.0 >> 40) as f32) / ((1u32 << 24) as f32)
    }

    fn next_index(&mut self, len: usize) -> usize {
        ((self.next_f32() * len as f32) as usize).min(len - 1)
    }
}

const MAX_ITERATIONS: usize = 24;
const CONVERGENCE_EPSILON: f32 = 1e-5;

/// k-means++ seeding, then Lloyd iterations. Returns at most `k` swatches,
/// heaviest first; fewer when the image has fewer distinct colours than `k`.
///
/// `pixels` should already exclude transparent pixels.
pub fn palette_from_pixels(pixels: &[Oklab], k: usize) -> Vec<Swatch> {
    if pixels.is_empty() || k == 0 {
        return Vec::new();
    }

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    // --- k-means++ seeding ---
    let mut centroids: Vec<Oklab> = Vec::with_capacity(k);
    centroids.push(pixels[rng.next_index(pixels.len())]);

    let mut dist_sq = vec![f32::INFINITY; pixels.len()];
    while centroids.len() < k {
        let newest = centroids[centroids.len() - 1];
        let mut total = 0.0f32;
        for (d, p) in dist_sq.iter_mut().zip(pixels) {
            *d = d.min(p.distance_sq(newest));
            total += *d;
        }
        // Every remaining pixel coincides with an existing centroid: the image
        // has fewer distinct colours than k, so stop seeding.
        if total <= f32::EPSILON {
            break;
        }
        let mut target = rng.next_f32() * total;
        let mut chosen = pixels.len() - 1;
        for (i, d) in dist_sq.iter().enumerate() {
            target -= *d;
            if target <= 0.0 {
                chosen = i;
                break;
            }
        }
        centroids.push(pixels[chosen]);
    }

    // --- Lloyd iterations ---
    let mut assignment = vec![0usize; pixels.len()];
    for _ in 0..MAX_ITERATIONS {
        for (slot, p) in assignment.iter_mut().zip(pixels) {
            let mut best = 0;
            let mut best_d = f32::INFINITY;
            for (i, c) in centroids.iter().enumerate() {
                let d = p.distance_sq(*c);
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            *slot = best;
        }

        let mut sums = vec![(0.0f32, 0.0f32, 0.0f32, 0usize); centroids.len()];
        for (idx, p) in assignment.iter().zip(pixels) {
            let s = &mut sums[*idx];
            s.0 += p.l;
            s.1 += p.a;
            s.2 += p.b;
            s.3 += 1;
        }

        let mut movement = 0.0f32;
        for (c, s) in centroids.iter_mut().zip(&sums) {
            if s.3 == 0 {
                continue;
            }
            let n = s.3 as f32;
            let next = Oklab {
                l: s.0 / n,
                a: s.1 / n,
                b: s.2 / n,
            };
            movement += c.distance_sq(next);
            *c = next;
        }
        if movement < CONVERGENCE_EPSILON {
            break;
        }
    }

    // --- Final counts, dropping empty clusters ---
    let mut counts = vec![0usize; centroids.len()];
    for idx in &assignment {
        counts[*idx] += 1;
    }

    let total = pixels.len() as f32;
    let mut out: Vec<Swatch> = centroids
        .iter()
        .zip(&counts)
        .filter(|(_, n)| **n > 0)
        .map(|(c, n)| Swatch {
            l: c.l,
            a: c.a,
            b: c.b,
            hex: hex_of(*c),
            weight: *n as f32 / total,
        })
        .collect();

    out.sort_by(|x, y| y.weight.total_cmp(&x.weight));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn white_and_black_land_at_expected_lightness() {
        let white = srgb_to_oklab(255, 255, 255);
        assert!(approx(white.l, 1.0, 1e-3), "white L was {}", white.l);
        assert!(approx(white.a, 0.0, 1e-3) && approx(white.b, 0.0, 1e-3));

        let black = srgb_to_oklab(0, 0, 0);
        assert!(approx(black.l, 0.0, 1e-3), "black L was {}", black.l);
    }

    #[test]
    fn oklab_roundtrips_through_srgb() {
        for (r, g, b) in [
            (0, 0, 0),
            (255, 255, 255),
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (18, 52, 86),
            (200, 137, 42),
        ] {
            let back = oklab_to_srgb(srgb_to_oklab(r, g, b));
            assert_eq!(back, (r, g, b), "roundtrip failed for {r},{g},{b}");
        }
    }

    #[test]
    fn solid_image_yields_one_swatch() {
        let pixels = vec![srgb_to_oklab(200, 30, 40); 500];
        let palette = palette_from_pixels(&pixels, 5);
        assert_eq!(palette.len(), 1, "got {palette:?}");
        assert_eq!(palette[0].hex, "#c81e28");
        assert!(approx(palette[0].weight, 1.0, 1e-6));
    }

    #[test]
    fn three_distinct_colours_are_separated_and_weight_ordered() {
        let mut pixels = vec![srgb_to_oklab(255, 0, 0); 60];
        pixels.extend(vec![srgb_to_oklab(0, 255, 0); 30]);
        pixels.extend(vec![srgb_to_oklab(0, 0, 255); 10]);

        let palette = palette_from_pixels(&pixels, 5);
        assert_eq!(palette.len(), 3, "got {palette:?}");

        // Descending weight, matching the 60/30/10 split.
        assert!(approx(palette[0].weight, 0.6, 1e-4));
        assert!(approx(palette[1].weight, 0.3, 1e-4));
        assert!(approx(palette[2].weight, 0.1, 1e-4));

        assert_eq!(palette[0].hex, "#ff0000");
        assert_eq!(palette[1].hex, "#00ff00");
        assert_eq!(palette[2].hex, "#0000ff");
    }

    #[test]
    fn palette_is_deterministic_across_runs() {
        let mut pixels = Vec::new();
        for i in 0..900u32 {
            pixels.push(srgb_to_oklab(
                (i % 256) as u8,
                ((i * 7) % 256) as u8,
                ((i * 13) % 256) as u8,
            ));
        }
        let first = palette_from_pixels(&pixels, 5);
        let second = palette_from_pixels(&pixels, 5);
        assert_eq!(first, second);
    }

    #[test]
    fn empty_input_is_not_a_panic() {
        assert!(palette_from_pixels(&[], 5).is_empty());
        assert!(palette_from_pixels(&[srgb_to_oklab(1, 2, 3)], 0).is_empty());
    }

    #[test]
    fn hex_parsing_handles_both_forms() {
        assert_eq!(parse_hex("#ff8800"), Some((255, 136, 0)));
        assert_eq!(parse_hex("ff8800"), Some((255, 136, 0)));
        assert_eq!(parse_hex("#f80"), Some((255, 136, 0)));
        assert_eq!(parse_hex("#ff88"), None);
        assert_eq!(parse_hex("zzzzzz"), None);
    }
}
