//! The Argus app icon, drawn in code so the repository carries no binary
//! artwork: a One Dark rounded square with a single open eye (Argus
//! Panoptes, the watchman of a hundred eyes).
//!
//! Shapes are evaluated in unit coordinates (0..1, y down) and every pixel
//! is supersampled, so each size is drawn crisp rather than scaled down.

/// Supersamples per pixel along each axis.
const SAMPLES: u32 = 4;

/// RGB colors of the layers, back to front.
const BACKGROUND_TOP: [f32; 3] = rgb(0x3a4150);
const BACKGROUND_BOTTOM: [f32; 3] = rgb(0x1e2228);
const LID: [f32; 3] = rgb(0x61afef);
const SCLERA: [f32; 3] = rgb(0xe6e9ef);
const IRIS: [f32; 3] = rgb(0x56b6c2);
const PUPIL: [f32; 3] = rgb(0x1e2228);
const GLINT: [f32; 3] = rgb(0xffffff);

/// Renders the icon at `size` × `size` pixels as straight-alpha RGBA.
pub fn render(size: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(size as usize * size as usize * 4);
    let step = 1.0 / (size * SAMPLES) as f32;
    for py in 0..size {
        for px in 0..size {
            // Premultiplied sum over the samples.
            let mut sum = [0.0f32; 4];
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = ((px * SAMPLES + sx) as f32 + 0.5) * step;
                    let y = ((py * SAMPLES + sy) as f32 + 0.5) * step;
                    if let Some(color) = sample(x, y) {
                        sum[0] += color[0];
                        sum[1] += color[1];
                        sum[2] += color[2];
                        sum[3] += 1.0;
                    }
                }
            }
            let coverage = sum[3] / (SAMPLES * SAMPLES) as f32;
            if sum[3] == 0.0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                for channel in &sum[..3] {
                    rgba.push(to_byte(channel / sum[3]));
                }
                rgba.push(to_byte(coverage));
            }
        }
    }
    rgba
}

/// The color at one point, or `None` outside the icon.
fn sample(x: f32, y: f32) -> Option<[f32; 3]> {
    // macOS icons sit on an 824-pixel body inside the 1024 canvas.
    let (cx, cy) = (x - 0.5, y - 0.5);
    let half = 412.0 / 1024.0;
    if !in_superellipse(cx / half, cy / half, 5.0) {
        return None;
    }
    let (ix, iy) = (cx - 0.035, cy + 0.035);
    let color = if (ix * ix + iy * iy).sqrt() < 0.028 {
        GLINT
    } else if (cx * cx + cy * cy).sqrt() < 0.062 {
        PUPIL
    } else if (cx * cx + cy * cy).sqrt() < 0.135 && in_lens(cx, cy, 0.30, 0.17) {
        IRIS
    } else if in_lens(cx, cy, 0.30, 0.17) {
        SCLERA
    } else if in_lens(cx, cy, 0.345, 0.215) {
        LID
    } else {
        let t = (y - 0.5 + half) / (2.0 * half);
        lerp(BACKGROUND_TOP, BACKGROUND_BOTTOM, t.clamp(0.0, 1.0))
    };
    Some(color)
}

/// Whether `(x, y)` (relative to the center) is inside the almond spanning
/// `±half_width` across and `±half_height` up and down: the intersection of
/// two circles whose centers sit above and below the middle.
fn in_lens(x: f32, y: f32, half_width: f32, half_height: f32) -> bool {
    let offset = (half_width * half_width - half_height * half_height) / (2.0 * half_height);
    let radius = offset + half_height;
    let inside = |center_y: f32| x * x + (y - center_y) * (y - center_y) <= radius * radius;
    inside(-offset) && inside(offset)
}

/// `|x|^n + |y|^n <= 1`: the continuous-corner square of macOS icons.
fn in_superellipse(x: f32, y: f32, n: f32) -> bool {
    x.abs().powf(n) + y.abs().powf(n) <= 1.0
}

fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

const fn rgb(hex: u32) -> [f32; 3] {
    [
        ((hex >> 16) & 0xFF) as f32 / 255.0,
        ((hex >> 8) & 0xFF) as f32 / 255.0,
        (hex & 0xFF) as f32 / 255.0,
    ]
}

fn to_byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}
