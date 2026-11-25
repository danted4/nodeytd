//! App icon, drawn in code (no image assets): a squircle plate with a
//! download arrow over a tray. Shared by the window icon, `build.rs` (the
//! Windows .ico) and the macOS bundler.

const PLATE: [u8; 4] = [20, 10, 14, 255];
const GLYPH: [u8; 4] = [255, 88, 102, 255];
const SAMPLES: i32 = 3;
const SQUIRCLE_N: f32 = 5.0;
const CONTENT_PAD: f32 = 0.16;

/// RGBA pixels, `size * size * 4` bytes.
pub fn raster(size: u32) -> Vec<u8> {
    let mut rgba = vec![0_u8; (size as usize) * (size as usize) * 4];
    let s = size as f32;
    for y in 0..size as i32 {
        for x in 0..size as i32 {
            let mut plate = 0.0_f32;
            let mut glyph = 0.0_f32;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                    if squircle(px, py, s) > 1.0 {
                        continue;
                    }
                    plate += 1.0;
                    if in_glyph(px, py, s) {
                        glyph += 1.0;
                    }
                }
            }
            if plate <= 0.0 {
                continue;
            }
            let mut color = lerp(PLATE, GLYPH, glyph / plate);
            color[3] = (plate / (SAMPLES * SAMPLES) as f32 * 255.0).round() as u8;
            let i = ((y as u32 * size + x as u32) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&color);
        }
    }
    rgba
}

fn squircle(x: f32, y: f32, size: f32) -> f32 {
    let c = size * 0.5;
    let nx = (x - c) / c;
    let ny = (y - c) / c;
    nx.abs().powf(SQUIRCLE_N) + ny.abs().powf(SQUIRCLE_N)
}

/// The glyph in a 256-unit box inside the plate's padding.
fn in_glyph(x: f32, y: f32, size: f32) -> bool {
    let pad = size * CONTENT_PAD;
    let inner = size - 2.0 * pad;
    if inner <= 0.0 {
        return false;
    }
    let u = (x - pad) / inner * 256.0;
    let v = (y - pad) / inner * 256.0;
    // Shaft.
    let shaft = (106.0..=150.0).contains(&u) && (28.0..=124.0).contains(&v);
    // Head: triangle from (52,116)-(204,116) down to the tip at (128,196).
    let head = if (116.0..=196.0).contains(&v) {
        let half = 76.0 * (196.0 - v) / 80.0;
        (u - 128.0).abs() <= half
    } else {
        false
    };
    // Tray: a U under the arrow.
    let base = (36.0..=220.0).contains(&u) && (214.0..=236.0).contains(&v);
    let walls = (176.0..=236.0).contains(&v)
        && ((36.0..=58.0).contains(&u) || (198.0..=220.0).contains(&u));
    shaft || head || base || walls
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
    fn corners_are_transparent_and_glyph_is_drawn() {
        let icon = raster(64);
        assert_eq!(icon.len(), 64 * 64 * 4);
        let alpha = |x: u32, y: u32| icon[((y * 64 + x) * 4 + 3) as usize];
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(63, 63), 0);
        assert!(icon
            .chunks(4)
            .any(|px| px[0] > 200 && px[1] < 140 && px[3] > 200));
        let mid = ((32 * 64 + 32) * 4) as usize;
        assert!(icon[mid + 3] > 200, "plate fills the centre");
    }
}
