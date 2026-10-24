//! where the screens go, left, top, width and height, in the size the GPU
//! lays them out in: 640x480 on the R36S, the panel's shape on others.

use crate::settings::Layout;

const TOP: [f32; 2] = [400.0, 240.0];
const BOTTOM: [f32; 2] = [320.0, 240.0];

/// the areas for the screens as the settings have them, or with a second
/// window to show one by itself, the other filling the panel and the one
/// there the whole of its own size, which the second window fills.
pub fn shown(settings: &crate::settings::Settings) -> [Option<[i32; 4]>; 2] {
    let size = zakuro_gpu::layout_size();
    if zakuro_gpu::stacked() {
        return stacked(settings, size);
    }
    if !zakuro_gpu::second_window() {
        return areas(settings.layout, settings.swapped, size);
    }
    zakuro_gpu::set_second_shows_top(settings.swapped);
    let (width, height) = (size.0 as f32, size.1 as f32);
    let (here, there) = if settings.swapped { (BOTTOM, TOP) } else { (TOP, BOTTOM) };
    let scale = (width / here[0]).min(height / here[1]);
    let (a, b) = ((here[0] * scale).round() as i32, (here[1] * scale).round() as i32);
    // the screen on the panel is the top one unless they are swapped, and
    // the second window stretches the other as the settings say
    let filled = if settings.stretched.stretches(!settings.swapped) {
        Some([0, 0, size.0 as i32, size.1 as i32])
    } else {
        Some([(size.0 as i32 - a) / 2, (size.1 as i32 - b) / 2, a, b])
    };
    zakuro_gpu::set_second_stretched(settings.stretched.stretches(settings.swapped));
    let alone = Some([0, 0, there[0] as i32, there[1] as i32]);
    if settings.swapped { [alone, filled] } else { [filled, alone] }
}

/// both panels of a dual-screen handheld as one window, stacked: a screen
/// on each half, the top one above unless swapped, each kept in shape or
/// stretched over its half as the settings say.
fn stacked(settings: &crate::settings::Settings, (width, height): (u32, u32)) -> [Option<[i32; 4]>; 2] {
    let (width, half) = (width as i32, height as i32 / 2);
    let place = |screen: [f32; 2], top: bool, stretched: bool| {
        let y = if top { 0 } else { half };
        if stretched {
            return Some([0, y, width, half]);
        }
        let scale = (width as f32 / screen[0]).min(half as f32 / screen[1]);
        let (a, b) = ((screen[0] * scale).round() as i32, (screen[1] * scale).round() as i32);
        Some([(width - a) / 2, y + (half - b) / 2, a, b])
    };
    // the 3DS's top screen, then its bottom one
    let top_above = !settings.swapped;
    [place(TOP, top_above, settings.stretched.stretches(true)), place(BOTTOM, !top_above, settings.stretched.stretches(false))]
}

pub fn areas(layout: Layout, swapped: bool, (width, height): (u32, u32)) -> [Option<[i32; 4]>; 2] {
    let (width, height) = (width as i32, height as i32);
    let (first, second) = if swapped { (BOTTOM, TOP) } else { (TOP, BOTTOM) };
    let size = |screen: [f32; 2], scale: f32| [(screen[0] * scale).round() as i32, (screen[1] * scale).round() as i32];
    // as big as fits across and down
    let fit = |across: f32, down: f32| (width as f32 / across).min(height as f32 / down);
    let [first, second] = match layout {
        // the first screen 1.3 times over, the second 0.7 times under it,
        // or both as they are
        Layout::BigTop | Layout::Original => {
            let (big, small) = if layout == Layout::BigTop { (1.3, 0.7) } else { (1.0, 1.0) };
            let scale = fit((first[0] * big).max(second[0] * small), 240.0 * (big + small));
            let ([a, b], [c, d]) = (size(first, big * scale), size(second, small * scale));
            let top = (height - b - d) / 2;
            [Some([(width - a) / 2, top, a, b]), Some([(width - c) / 2, top + b, c, d])]
        }
        Layout::TopOnly => {
            let [a, b] = size(first, fit(first[0], first[1]));
            [Some([(width - a) / 2, (height - b) / 2, a, b]), None]
        }
        // both across the panel
        Layout::SideBySide => {
            let scale = fit(first[0] + second[0], 240.0);
            let ([a, b], [c, d]) = (size(first, scale), size(second, scale));
            let left = (width - a - c) / 2;
            [Some([left, (height - b) / 2, a, b]), Some([left + a, (height - d) / 2, c, d])]
        }
        // the first screen big, the second half its size beside it, level
        // with its top
        Layout::BigBeside => {
            let scale = fit(first[0] + second[0] / 2.0, first[1]);
            let ([a, b], [c, d]) = (size(first, scale), size(second, scale / 2.0));
            let (left, top) = ((width - a - c) / 2, (height - b) / 2);
            [Some([left, top, a, b]), Some([left + a, top, c, d])]
        }
    };
    if swapped { [second, first] } else { [first, second] }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the R36S's 640x480 keeps the places the layouts always had.
    #[test]
    fn r36s_places_stay() {
        let r36s = (640, 480);
        let cases = [
            (Layout::BigTop, false, [Some([60, 0, 520, 312]), Some([208, 312, 224, 168])]),
            (Layout::BigTop, true, [Some([180, 312, 280, 168]), Some([112, 0, 416, 312])]),
            (Layout::TopOnly, false, [Some([0, 48, 640, 384]), None]),
            (Layout::TopOnly, true, [None, Some([0, 0, 640, 480])]),
            (Layout::SideBySide, false, [Some([0, 133, 356, 213]), Some([356, 133, 284, 213])]),
            (Layout::SideBySide, true, [Some([284, 133, 356, 213]), Some([0, 133, 284, 213])]),
            (Layout::Original, false, [Some([120, 0, 400, 240]), Some([160, 240, 320, 240])]),
            (Layout::Original, true, [Some([120, 240, 400, 240]), Some([160, 0, 320, 240])]),
            (Layout::BigBeside, false, [Some([0, 103, 457, 274]), Some([457, 103, 183, 137])]),
        ];
        for (layout, swapped, expected) in cases {
            assert_eq!(areas(layout, swapped, r36s), expected, "{layout:?} swapped {swapped}");
        }
    }

    /// DualStack's 640x960 window, both RG DS panels: the top screen as big
    /// as it fits on the top panel, the bottom one exactly twice its size on
    /// the bottom panel, which the swap and stretching move and fill.
    #[test]
    fn stacked_panels_get_a_screen_each() {
        use crate::settings::{Settings, Stretch};
        let rg_ds = (640, 960);
        let mut settings = Settings::default();
        assert_eq!(stacked(&settings, rg_ds), [Some([0, 48, 640, 384]), Some([0, 480, 640, 480])]);
        settings.swapped = true;
        assert_eq!(stacked(&settings, rg_ds), [Some([0, 528, 640, 384]), Some([0, 0, 640, 480])]);
        settings.swapped = false;
        settings.stretched = Stretch::Top;
        assert_eq!(stacked(&settings, rg_ds), [Some([0, 0, 640, 480]), Some([0, 480, 640, 480])]);
    }
}