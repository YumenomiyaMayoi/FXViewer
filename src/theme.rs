//! Single source of truth for the terminal palette.

use eframe::egui::Color32;

pub const BG: Color32 = Color32::from_rgb(0x0E, 0x11, 0x17);
pub const PANEL: Color32 = Color32::from_rgb(0x13, 0x17, 0x22);
pub const SURFACE: Color32 = Color32::from_rgb(0x1C, 0x22, 0x30);
pub const LINE: Color32 = Color32::from_rgb(0x23, 0x2A, 0x38);
pub const GRID: Color32 = Color32::from_rgb(0x1B, 0x21, 0x30);

pub const TEXT: Color32 = Color32::from_rgb(0xE4, 0xE7, 0xEE);
pub const TEXT2: Color32 = Color32::from_rgb(0xD1, 0xD4, 0xDC);
pub const MUTED: Color32 = Color32::from_rgb(0x78, 0x7B, 0x86);
pub const DIM: Color32 = Color32::from_rgb(0x4C, 0x52, 0x5E);

/// Rising value. Red follows the convention used on Chinese markets.
pub const UP: Color32 = Color32::from_rgb(0xEF, 0x53, 0x50);
pub const DOWN: Color32 = Color32::from_rgb(0x26, 0xA6, 0x9A);
/// A candle that closed exactly where it opened. Neither up nor down. A flat
/// bucket is common — an idle GPU never changes within its 1 s counter
/// resolution — and painting those red made a quiet instrument look like a
/// wall of buying.
pub const FLAT: Color32 = Color32::from_rgb(0x6B, 0x72, 0x82);

pub const MA5: Color32 = Color32::from_rgb(0xF0, 0xA5, 0x00);
pub const MA20: Color32 = Color32::from_rgb(0x29, 0x62, 0xFF);
pub const AMBER: Color32 = Color32::from_rgb(0xBA, 0x75, 0x17);

pub fn load_level(v: f32) -> Color32 {
    if v >= 75.0 {
        UP
    } else if v >= 40.0 {
        AMBER
    } else {
        DOWN
    }
}
