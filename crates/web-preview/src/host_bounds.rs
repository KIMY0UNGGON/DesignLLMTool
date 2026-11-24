use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{SHOW_WINDOW_CMD, SW_SHOWMINNOACTIVE, SW_SHOWNA, WINDOWPLACEMENT, WINDOWPLACEMENT_FLAGS};

pub(super) fn show_without_activation(show: u32) -> SHOW_WINDOW_CMD {
    if show == 2 { SW_SHOWMINNOACTIVE } else { SW_SHOWNA }
}

pub(super) fn apply_placement(placement: &mut WINDOWPLACEMENT, normal: RECT, show: u32, flags: u32) {
    placement.rcNormalPosition = normal;
    placement.flags = WINDOWPLACEMENT_FLAGS(flags);
    placement.showCmd = show_without_activation(show).0 as u32;
}

pub(super) fn parse_placement(value: &str) -> Option<(RECT, u32, u32)> {
    if value.len() > 128 { return None; }
    let mut parts = value.split(',');
    let rect = RECT { left: parts.next()?.parse().ok()?, top: parts.next()?.parse().ok()?, right: parts.next()?.parse().ok()?, bottom: parts.next()?.parse().ok()? };
    let show: u32 = parts.next()?.parse().ok()?;
    let flags: u32 = parts.next()?.parse().ok()?;
    let width = i64::from(rect.right) - i64::from(rect.left);
    let height = i64::from(rect.bottom) - i64::from(rect.top);
    (parts.next().is_none() && (1..=i64::from(i32::MAX)).contains(&width) && (1..=i64::from(i32::MAX)).contains(&height)
        && matches!(show, 1..=3) && (flags == 0 || (show == 2 && flags == 2))).then_some((rect, show, flags))
}

pub(super) fn clamp_rect(rect: RECT, work: RECT) -> Option<RECT> {
    let (left, top, right, bottom) = (i64::from(work.left), i64::from(work.top), i64::from(work.right), i64::from(work.bottom));
    if right <= left || bottom <= top { return None; }
    let width = (i64::from(rect.right) - i64::from(rect.left)).clamp(1, (right - left).min(i64::from(i32::MAX)));
    let height = (i64::from(rect.bottom) - i64::from(rect.top)).clamp(1, (bottom - top).min(i64::from(i32::MAX)));
    let x = i64::from(rect.left).clamp(left, right - width);
    let y = i64::from(rect.top).clamp(top, bottom - height);
    Some(RECT { left: x as i32, top: y as i32, right: (x + width) as i32, bottom: (y + height) as i32 })
}

pub(super) fn initial_rect(owner: RECT, work: RECT) -> Option<RECT> {
    let (left, top, right, bottom) = (i64::from(work.left), i64::from(work.top), i64::from(work.right), i64::from(work.bottom));
    let (width, height) = (right - left, bottom - top);
    if width <= 0 || height <= 0 || width > i64::from(i32::MAX) || height > i64::from(i32::MAX) { return None; }
    let left_free = (i64::from(owner.left) - left).clamp(0, width);
    let right_free = (right - i64::from(owner.right)).clamp(0, width);
    let min_width = width.min(320);
    let preferred = ((width - 1) / 2).max(min_width).min(width);
    let use_right = if left_free.max(right_free) >= min_width { right_free >= left_free } else { true };
    let free = if use_right { right_free } else { left_free };
    let window_width = if free >= min_width { preferred.min(free) } else { preferred };
    let x = if use_right { right - window_width } else { left };
    Some(RECT { left: x as i32, top: top as i32, right: (x + window_width) as i32, bottom: bottom as i32 })
}

pub(super) fn workspace_rect(rect: RECT, monitor: RECT, work: RECT) -> Option<RECT> {
    let dx = i64::from(work.left) - i64::from(monitor.left);
    let dy = i64::from(work.top) - i64::from(monitor.top);
    Some(RECT {
        left: (i64::from(rect.left) - dx).try_into().ok()?, top: (i64::from(rect.top) - dy).try_into().ok()?,
        right: (i64::from(rect.right) - dx).try_into().ok()?, bottom: (i64::from(rect.bottom) - dy).try_into().ok()?,
    })
}

pub(super) fn sync_bounds<E>(last: &mut (i32, i32), current: (i32, i32), apply: impl FnOnce(RECT) -> Result<(), E>) -> Result<(), E> {
    if *last != current {
        apply(RECT { left: 0, top: 0, right: current.0, bottom: current.1 })?;
        *last = current;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handed_over_rect_keeps_placement_and_clamps_to_work_area() {
        let work = RECT { left: -1920, top: 0, right: 0, bottom: 1040 };
        let original = parse_placement("-1500,100,-700,700,1,0").unwrap().0;
        assert_eq!(clamp_rect(original, work), Some(original));
        let offscreen = parse_placement("-2500,-200,500,1500,2,2").unwrap().0;
        assert_eq!(clamp_rect(offscreen, work), Some(work));
        for value in ["0,0,0,100,1,0", "1,2,3,4,5", "1,2,3,4,4,0", "1,2,3,4,1,2", "-2147483648,0,2147483647,1,1,0", "1,2,3,4,2,4"] {
            assert!(parse_placement(value).is_none(), "{value}");
        }
        assert!(parse_placement(&"1".repeat(129)).is_none());
        for (show, flags) in [(1, 0), (2, 0), (2, 2), (3, 0)] {
            assert_eq!(parse_placement(&format!("-1500,100,-700,700,{show},{flags}")), Some((original, show, flags)));
        }
        assert!(clamp_rect(original, RECT::default()).is_none());
    }

    #[test]
    fn initial_position_uses_available_side_and_fits_small_monitors() {
        let work = RECT { left: 0, top: 0, right: 1024, bottom: 728 };
        assert_eq!(initial_rect(RECT { left: 0, top: 0, right: 1024, bottom: 728 }, work), Some(RECT { left: 513, top: 0, right: 1024, bottom: 728 }));
        assert_eq!(initial_rect(RECT { left: 512, top: 0, right: 1024, bottom: 728 }, work), Some(RECT { left: 0, top: 0, right: 511, bottom: 728 }));
        assert_eq!(initial_rect(RECT { left: 200, top: 0, right: 1000, bottom: 728 }, work), Some(RECT { left: 513, top: 0, right: 1024, bottom: 728 }));
        let tiny = RECT { left: -200, top: -100, right: 0, bottom: 50 };
        assert_eq!(initial_rect(tiny, tiny), Some(tiny));
        assert!(initial_rect(work, RECT::default()).is_none());
    }

    #[test]
    fn restored_screen_rect_converts_to_workspace_coordinates() {
        let monitor = RECT { left: -1920, top: 0, right: 0, bottom: 1080 };
        let work = RECT { left: -1880, top: 30, right: 0, bottom: 1080 };
        let screen = RECT { left: -1800, top: 100, right: -800, bottom: 800 };
        assert_eq!(workspace_rect(screen, monitor, work), Some(RECT { left: -1840, top: 70, right: -840, bottom: 770 }));
        let secondary = RECT { left: 1920, top: -200, right: 3200, bottom: 824 };
        let top_taskbar = RECT { left: 1920, top: -160, right: 3200, bottom: 824 };
        let screen = RECT { left: 2000, top: -140, right: 2600, bottom: 540 };
        assert_eq!(workspace_rect(screen, secondary, top_taskbar), Some(RECT { left: 2000, top: -180, right: 2600, bottom: 500 }));
    }

    #[test]
    fn restored_placement_keeps_normal_rect_and_window_state() {
        let normal = RECT { left: -1800, top: 70, right: -800, bottom: 770 };
        for (show, flags, expected) in [(1, 0, SW_SHOWNA), (2, 0, SW_SHOWMINNOACTIVE), (2, 2, SW_SHOWMINNOACTIVE), (3, 0, SW_SHOWNA)] {
            let mut placement = WINDOWPLACEMENT::default();
            apply_placement(&mut placement, normal, show, flags);
            assert_eq!(placement.rcNormalPosition, normal);
            assert_eq!(placement.showCmd, expected.0 as u32);
            assert_eq!(placement.flags.0, flags);
            assert_eq!(show_without_activation(show), if show == 2 { SW_SHOWMINNOACTIVE } else { SW_SHOWNA });
        }
    }

    #[test]
    fn resize_during_initialization_is_applied_and_failure_can_retry() {
        let mut last = (800, 600);
        let current = (1100, 700);
        assert_eq!(sync_bounds(&mut last, current, |_| Err("failed")), Err("failed"));
        assert_eq!(last, (800, 600));
        let mut applied = None;
        sync_bounds(&mut last, current, |rect| { applied = Some(rect); Ok::<_, ()>(()) }).unwrap();
        let rect = applied.unwrap();
        assert_eq!((rect.right, rect.bottom), current);
        assert_eq!(last, current);
    }

    #[test]
    fn first_bounds_sync_applies_even_without_a_resize() {
        let mut last = (0, 0);
        let mut applied = None;
        sync_bounds(&mut last, (800, 600), |rect| { applied = Some(rect); Ok::<_, ()>(()) }).unwrap();
        assert_eq!(applied, Some(RECT { left: 0, top: 0, right: 800, bottom: 600 }));
    }
}
