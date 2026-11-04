// Icon artwork, drawn from maths so the build carries no binary assets.
//
// Pure `std`, no external crates: this file is compiled twice — once as a
// module of the app (the runtime window icon) and once `include!`-style by
// `build.rs`, which rasterises it into the .ico that becomes the executable's
// file icon.
//
// Coordinates live in a 64x64 design space and are scaled to whatever size is
// requested, so the same shape produces crisp 16px and 256px bitmaps.

#[allow(dead_code)]
pub const BG: [u8; 4] = [0x0E, 0x11, 0x17, 0xFF];
#[allow(dead_code)]
pub const UP: [u8; 4] = [0xEF, 0x53, 0x50, 0xFF];
#[allow(dead_code)]
pub const DOWN: [u8; 4] = [0x26, 0xA6, 0x9A, 0xFF];

/// (left, right, body top, body bottom, wick top, wick bottom, colour)
#[allow(dead_code)]
const CANDLES: [(f32, f32, f32, f32, f32, f32, [u8; 4]); 4] = [
    (9.0, 17.0, 42.0, 53.0, 37.0, 57.0, UP),
    (21.0, 29.0, 32.0, 46.0, 27.0, 51.0, DOWN),
    (33.0, 41.0, 24.0, 38.0, 19.0, 43.0, UP),
    (45.0, 53.0, 12.0, 29.0, 7.0, 33.0, UP),
];

/// Rasterises the icon at `size` x `size`. Returns straight (non-premultiplied)
/// RGBA, row-major, top-down.
#[allow(dead_code)]
pub fn render(size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; size * size * 4];
    if size == 0 {
        return buf;
    }
    let s = size as f32 / 64.0;

    // Rounded plate. Anti-aliased, which matters at 128/256 where the corners
    // are long enough for stair-stepping to show.
    let (p0, p1, r) = (2.0 * s, 62.0 * s, 13.0 * s);
    for y in 0..size {
        for x in 0..size {
            let d = round_rect_sdf(x as f32 + 0.5, y as f32 + 0.5, p0, p0, p1, p1, r);
            let cov = (0.5 - d).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            let i = (y * size + x) * 4;
            buf[i..i + 3].copy_from_slice(&BG[..3]);
            buf[i + 3] = (cov * 255.0).round() as u8;
        }
    }

    // Candles. Edges snap to whole pixels so thin wicks stay solid instead of
    // dissolving into grey when the target is 16px.
    let e = |v: f32| (v * s).round() as i32;
    let wick_w = ((2.0 * s).round() as i32).max(1);
    for &(l, r, bt, bb, wt, wb, c) in CANDLES.iter() {
        let cx = e((l + r) * 0.5);
        let wx = cx - wick_w / 2;
        fill(&mut buf, size, wx, e(wt), wx + wick_w, e(wb), c);
        fill(&mut buf, size, e(l), e(bt), e(r), e(bb), c);
    }

    buf
}

fn fill(buf: &mut [u8], size: usize, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 4]) {
    let hi = size as i32;
    for y in y0.max(0)..y1.min(hi) {
        for x in x0.max(0)..x1.min(hi) {
            let i = (y as usize * size + x as usize) * 4;
            buf[i..i + 4].copy_from_slice(&c);
        }
    }
}

/// Signed distance to a rounded rectangle: negative inside.
fn round_rect_sdf(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let cx = (x0 + x1) * 0.5;
    let cy = (y0 + y1) * 0.5;
    let hx = (x1 - x0) * 0.5 - r;
    let hy = (y1 - y0) * 0.5 - r;
    let qx = (px - cx).abs() - hx;
    let qy = (py - cy).abs() - hy;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}
