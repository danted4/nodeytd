//! App icon, drawn in code (no image assets): a squircle plate with a "Y"
//! sitting in a download tray. Same geometry as torrenter's icon (squircle,
//! padding, 3x3 samples, two-tone glyph); ytd's coral colours. Shared by the
//! window icon, `build.rs` (the Windows .ico) and the macOS bundler.

const PLATE: [u8; 4] = [20, 10, 14, 255];
/// The Y.
const BODY: [u8; 4] = [255, 88, 102, 255];
/// The tray: the pale accent, the part torrenter's pole caps play.
const ACCENT: [u8; 4] = [255, 214, 218, 255];
const SAMPLES: i32 = 3;
const SQUIRCLE_N: f32 = 5.0;
const CONTENT_PAD: f32 = 0.18;
/// Half the stroke width, in the glyph's 256-unit box (torrenter: 44 wide).
const HALF_STROKE: f32 = 22.0;

/// RGBA pixels, `size * size * 4` bytes.
pub fn raster(size: u32) -> Vec<u8> {
    let mut rgba = vec![0_u8; (size as usize) * (size as usize) * 4];
    let s = size as f32;
    for y in 0..size as i32 {
        for x in 0..size as i32 {
            let mut plate = 0.0_f32;
            let mut body = 0.0_f32;
            let mut accent = 0.0_f32;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                    if squircle(px, py, s) > 1.0 {
                        continue;
                    }
                    plate += 1.0;
                    match glyph_part(px, py, s) {
                        Part::Body => body += 1.0,
                        Part::Accent => accent += 1.0,
                        Part::None => {}
                    }
                }
            }
            if plate <= 0.0 {
                continue;
            }
            let mut color = PLATE;
            if body > 0.0 {
                color = lerp(color, BODY, body / plate);
            }
            if accent > 0.0 {
                color = lerp(color, ACCENT, accent / plate);
            }
            color[3] = (plate / (SAMPLES * SAMPLES) as f32 * 255.0).round() as u8;
            let i = ((y as u32 * size + x as u32) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&color);
        }
    }
    rgba
}

enum Part {
    None,
    Body,
    Accent,
}

fn squircle(x: f32, y: f32, size: f32) -> f32 {
    let c = size * 0.5;
    let nx = (x - c) / c;
    let ny = (y - c) / c;
    nx.abs().powf(SQUIRCLE_N) + ny.abs().powf(SQUIRCLE_N)
}

/// Distance from (u, v) to the segment a-b.
fn to_segment(u: f32, v: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((u - a.0) * dx + (v - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((u - a.0 - t * dx).powi(2) + (v - a.1 - t * dy).powi(2)).sqrt()
}

/// The glyph in a 256-unit box inside the plate's padding.
fn glyph_part(x: f32, y: f32, size: f32) -> Part {
    let pad = size * CONTENT_PAD;
    let inner = size - 2.0 * pad;
    if inner <= 0.0 {
        return Part::None;
    }
    let u = (x - pad) / inner * 256.0;
    let v = (y - pad) / inner * 256.0;
    // Tray: a U under the Y.
    let base = (36.0..=220.0).contains(&u) && (214.0..=236.0).contains(&v);
    let walls = (176.0..=236.0).contains(&v)
        && ((36.0..=58.0).contains(&u) || (198.0..=220.0).contains(&u));
    if base || walls {
        return Part::Accent;
    }
    // Y: two arms meeting at (128, 104), flat-topped, then the stem.
    let fork = (128.0, 104.0);
    let arms = v >= 28.0
        && (to_segment(u, v, (60.0, 28.0), fork) <= HALF_STROKE
            || to_segment(u, v, (196.0, 28.0), fork) <= HALF_STROKE);
    let stem = (106.0..=150.0).contains(&u) && (90.0..=180.0).contains(&v);
    if arms || stem {
        Part::Body
    } else {
        Part::None
    }
}

fn lerp(a: [u8; 4], b: [u8; 4], t: f32) -> [u8; 4] {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t).round() as u8;
    [mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2]), 255]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_transparent_and_both_tones_are_drawn() {
        let icon = raster(64);
        assert_eq!(icon.len(), 64 * 64 * 4);
        let alpha = |x: u32, y: u32| icon[((y * 64 + x) * 4 + 3) as usize];
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(63, 63), 0);
        assert!(
            icon.chunks(4)
                .any(|px| px[0] > 200 && px[1] < 140 && px[3] > 200),
            "coral Y"
        );
        assert!(
            icon.chunks(4)
                .any(|px| px[0] > 230 && px[1] > 190 && px[2] > 190 && px[3] > 200),
            "pale tray"
        );
        let mid = ((32 * 64 + 32) * 4) as usize;
        assert!(icon[mid + 3] > 200, "plate fills the centre");
        assert!(
            icon[mid] > 200 && icon[mid + 1] < 140,
            "the stem runs through the centre"
        );
    }
}
