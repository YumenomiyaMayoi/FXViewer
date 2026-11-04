// Runtime window icon, derived from the shared artwork in `icon_art`.

use eframe::egui::IconData;

use crate::icon_art;

/// Size handed to the window manager. 64 is the largest the compositor
/// normally asks for on the taskbar; it upscales cleanly.
pub const SIZE: usize = 64;

pub fn app_icon() -> IconData {
    IconData {
        rgba: icon_art::render(SIZE),
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_shape_is_sane() {
        let icon = app_icon();
        assert_eq!(icon.rgba.len(), SIZE * SIZE * 4);

        for (x, y) in [(0usize, 0usize), (SIZE - 1, 0), (0, SIZE - 1), (SIZE - 1, SIZE - 1)] {
            assert_eq!(icon.rgba[(y * SIZE + x) * 4 + 3], 0, "corner {x},{y} not transparent");
        }
        let c = ((SIZE / 2) * SIZE + SIZE / 2) * 4;
        assert_eq!(icon.rgba[c + 3], 255, "plate should be opaque");

        let red = icon.rgba.chunks(4).filter(|p| p[..3] == icon_art::UP[..3]).count();
        let green = icon.rgba.chunks(4).filter(|p| p[..3] == icon_art::DOWN[..3]).count();
        assert!(red > 300, "red candles too small: {red}");
        assert!(green > 100, "green candle too small: {green}");
    }

    /// Every size the .ico carries has to rasterise without panicking and
    /// still show both candle colours — a 16px icon that collapses to a blank
    /// plate would look broken in Explorer's list view.
    #[test]
    fn all_icon_sizes_render() {
        for size in [16usize, 24, 32, 48, 64, 128, 256] {
            let px = icon_art::render(size);
            assert_eq!(px.len(), size * size * 4);

            let red = px.chunks(4).filter(|p| p[..3] == icon_art::UP[..3]).count();
            let green = px.chunks(4).filter(|p| p[..3] == icon_art::DOWN[..3]).count();
            assert!(red > size * size / 100, "{size}px: red too small ({red})");
            assert!(green > size * size / 400, "{size}px: green too small ({green})");

            let alpha = px.chunks(4).filter(|p| p[3] > 0).count();
            assert!(alpha > size * size / 2, "{size}px: plate too small ({alpha})");
        }
    }
}
