//! Where the `--smoke-test` window sits (task 65 r2). eframe shows the root window after its first frame whatever the
//! viewport says (`EpiIntegration::post_rendering` calls `set_visible(true)`), so a hidden smoke window still appeared
//! on the user's screen. It is created outside the virtual screen instead, at the capture size, so it is shown and
//! keeps drawing (the capture needs frames) but lies on no monitor.

use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN};

/// The outer position, in egui points, of a smoke window of `size` points.
pub(crate) fn offscreen_position(size: [f32; 2]) -> [f32; 2] {
    // SAFETY: GetSystemMetrics takes an index and only reads system values.
    let (left, top) = unsafe { (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN)) };
    before_virtual_screen(left as f32, top as f32, size)
}

/// The virtual screen's left and top are 0 or less (the primary monitor starts at 0, 0). They come in this process's
/// DPI units and the window is placed in points, so the margins are wide: twice the edge, twice the size and 1000
/// more keep the window's far edge before the virtual screen for window scales down to half the system scale.
fn before_virtual_screen(left: f32, top: f32, size: [f32; 2]) -> [f32; 2] {
    [2.0 * left - 2.0 * size[0] - 1000.0, 2.0 * top - 2.0 * size[1] - 1000.0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smoke_window_ends_before_the_left_and_top_of_the_virtual_screen() {
        let size = [1440.0, 900.0];
        for (left, top) in [(0.0, 0.0), (-2560.0, -1440.0)] {
            let [x, y] = before_virtual_screen(left, top, size);
            // The far edges end 1000 points before the edges, and the twice-edge term keeps a monitor at the left or
            // top (a negative edge) clear when the window's scale is half the system's.
            assert!(x + size[0] <= left - 1000.0 && y + size[1] <= top - 1000.0, "{left},{top}");
            assert!((x + size[0]) * 0.5 <= left && (y + size[1]) * 0.5 <= top, "{left},{top}");
        }
        let [x, y] = offscreen_position([800.0, 560.0]);
        assert!(x <= -2600.0 && y <= -2120.0, "{x},{y}");
    }
}
