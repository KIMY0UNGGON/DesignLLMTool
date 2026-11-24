use windows::{
    core::BOOL,
    Win32::{
        Foundation::{HWND, LPARAM, RECT},
        Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST},
        UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowPlacement, GetWindowThreadProcessId, IsWindow, IsWindowVisible, SetForegroundWindow, GW_HWNDPREV, WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED},
    },
};

pub(super) fn foreground_to_preserve(old_pid: Option<u32>) -> Option<isize> {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() { return None; }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(foreground, Some(&mut pid)); }
    (Some(pid) != old_pid).then_some(foreground.0 as isize)
}

#[derive(Debug, PartialEq)]
enum FocusAction { Wait, Restore, Done }

fn restoration_finished(ready: bool, restored: bool) -> bool { !restored || ready }

fn focus_action(anchor: isize, foreground: isize, is_preview: bool, ready: bool, valid: bool, expired: bool) -> FocusAction {
    if !valid || expired { FocusAction::Done }
    else if foreground == 0 { FocusAction::Wait }
    else if foreground == anchor { if ready { FocusAction::Done } else { FocusAction::Wait } }
    else if is_preview { FocusAction::Restore }
    else { FocusAction::Done }
}

pub(super) fn preserve_foreground(anchor: isize, preview_pid: u32, ready: bool, expired: bool) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    let mut pid = 0;
    if !foreground.0.is_null() { unsafe { GetWindowThreadProcessId(foreground, Some(&mut pid)); } }
    match focus_action(anchor, foreground.0 as isize, pid == preview_pid, ready, unsafe { IsWindow(Some(HWND(anchor as *mut _))).as_bool() }, expired) {
        FocusAction::Wait => false,
        FocusAction::Done => true,
        FocusAction::Restore => restoration_finished(ready, unsafe { SetForegroundWindow(HWND(anchor as *mut _)).as_bool() }),
    }
}

fn placement_monitor(hwnd: HWND, placement: &WINDOWPLACEMENT) -> HMONITOR {
    if placement.showCmd == 2 {
        unsafe { MonitorFromRect(&placement.rcNormalPosition, MONITOR_DEFAULTTONEAREST) }
    } else { unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) } }
}

pub(super) fn window_placement(pid: u32, editor: Option<isize>) -> Option<(RECT, u32, u32, Option<(isize, u32)>, Option<(isize, u32)>)> {
    struct Search { pid: u32, editor: Option<isize>, placement: Option<(RECT, u32, u32, Option<(isize, u32)>, Option<(isize, u32)>)> }
    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> BOOL {
        let search = unsafe { &mut *(param.0 as *mut Search) };
        let mut pid = 0;
        let mut class = [0; 64];
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)); }
        let length = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
        if pid == search.pid && unsafe { IsWindowVisible(hwnd).as_bool() }
            && class[..length].iter().copied().eq("DesignLLMWebPreviewHost".encode_utf16()) {
            let mut placement = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            let mut monitor = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if unsafe { GetWindowPlacement(hwnd, &mut placement) }.is_ok() {
                let display = placement_monitor(hwnd, &placement);
                if !unsafe { GetMonitorInfoW(display, &mut monitor).as_bool() } { return true.into(); }
                let r = placement.rcNormalPosition;
                let screen = screen_rect(r, monitor.rcMonitor, monitor.rcWork);
                let flags = if placement.showCmd == 2 { (placement.flags & WPF_RESTORETOMAXIMIZED).0 } else { 0 };
                let previous = unsafe { GetWindow(hwnd, GW_HWNDPREV) }.ok();
                let mut cursor = previous;
                let mut editor_above = None;
                for _ in 0..1024 {
                    let Some(window) = cursor else { break; };
                    if Some(window.0 as isize) == search.editor {
                        let mut pid = 0;
                        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)); }
                        editor_above = Some((window.0 as isize, pid));
                        break;
                    }
                    cursor = unsafe { GetWindow(window, GW_HWNDPREV) }.ok();
                }
                let previous = previous.map(|window| {
                    let mut pid = 0;
                    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)); }
                    (window.0 as isize, pid)
                });
                search.placement = screen.map(|r| (r, placement.showCmd, flags, previous, editor_above));
                return false.into();
            }
        }
        true.into()
    }
    let mut search = Search { pid, editor, placement: None };
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut _ as isize)) };
    search.placement
}

fn screen_rect(rect: RECT, monitor: RECT, work: RECT) -> Option<RECT> {
    let dx = i64::from(work.left) - i64::from(monitor.left);
    let dy = i64::from(work.top) - i64::from(monitor.top);
    Some(RECT {
        left: (i64::from(rect.left) + dx).try_into().ok()?, top: (i64::from(rect.top) + dy).try_into().ok()?,
        right: (i64::from(rect.right) + dx).try_into().ok()?, bottom: (i64::from(rect.bottom) + dy).try_into().ok()?,
    })
}

pub(super) fn hand_over(command: &mut std::process::Command, placement: Option<(RECT, u32, u32, Option<(isize, u32)>, Option<(isize, u32)>)>, refresh: bool) {
    command.env_remove("DESIGNLLM_PREVIEW_Z_AFTER");
    if let Some((r, mut show, mut flags, previous, editor)) = placement {
        if !refresh && show == 2 {
            show = if flags & WPF_RESTORETOMAXIMIZED.0 != 0 { 3 } else { 1 };
            flags = 0;
        }
        command.env("DESIGNLLM_PREVIEW_RECT", format!("{},{},{},{},{},{}", r.left, r.top, r.right, r.bottom, show, flags));
        if refresh {
            let (hwnd, pid) = previous.unwrap_or((0, 0));
            let (editor, editor_pid) = editor.unwrap_or((0, 0));
            command.env("DESIGNLLM_PREVIEW_Z_AFTER", format!("{hwnd},{pid},{editor},{editor_pid}"));
        }
    } else {
        command.env_remove("DESIGNLLM_PREVIEW_RECT");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_passes_old_rect_and_clears_missing_rect() {
        let mut command = std::process::Command::new("unused");
        for (show, flags) in [(1, 0), (2, 0), (2, 2), (3, 0)] {
            hand_over(&mut command, Some((RECT { left: -1500, top: 100, right: -700, bottom: 700 }, show, flags, Some((42, 7)), None)), true);
            let value = command.get_envs().find(|(key, _)| *key == "DESIGNLLM_PREVIEW_RECT").unwrap().1.unwrap();
            assert_eq!(value, std::ffi::OsStr::new(&format!("-1500,100,-700,700,{show},{flags}")));
        }
        hand_over(&mut command, None, true);
        assert!(command.get_envs().find(|(key, _)| *key == "DESIGNLLM_PREVIEW_RECT").unwrap().1.is_none());
        assert!(command.get_envs().find(|(key, _)| *key == "DESIGNLLM_PREVIEW_Z_AFTER").unwrap().1.is_none());
    }

    #[test]
    fn explicit_open_restores_minimized_state_and_refresh_preserves_order() {
        let rect = RECT { left: 1, top: 2, right: 301, bottom: 402 };
        for (refresh, flags, expected) in [(false, 0, "1,2,301,402,1,0"), (false, 2, "1,2,301,402,3,0"),
            (true, 0, "1,2,301,402,2,0"), (true, 2, "1,2,301,402,2,2")] {
            let mut command = std::process::Command::new("unused");
            hand_over(&mut command, Some((rect, 2, flags, Some((42, 7)), Some((43, 8)))), refresh);
            assert_eq!(command.get_envs().find(|(k, _)| *k == "DESIGNLLM_PREVIEW_RECT").unwrap().1.unwrap(), std::ffi::OsStr::new(expected));
            let z = command.get_envs().find(|(k, _)| *k == "DESIGNLLM_PREVIEW_Z_AFTER").unwrap().1;
            assert_eq!(z, refresh.then_some(std::ffi::OsStr::new("42,7,43,8")));
        }
        let mut command = std::process::Command::new("unused");
        hand_over(&mut command, Some((rect, 1, 0, None, None)), true);
        assert_eq!(command.get_envs().find(|(k, _)| *k == "DESIGNLLM_PREVIEW_Z_AFTER").unwrap().1.unwrap(), std::ffi::OsStr::new("0,0,0,0"));
    }

    #[test]
    fn normal_placement_accounts_for_taskbar_offset() {
        let normal = RECT { left: -1840, top: 70, right: -840, bottom: 770 };
        let monitor = RECT { left: -1920, top: 0, right: 0, bottom: 1080 };
        let work = RECT { left: -1880, top: 30, right: 0, bottom: 1080 };
        assert_eq!(screen_rect(normal, monitor, work), Some(RECT { left: -1800, top: 100, right: -800, bottom: 800 }));
        let secondary = RECT { left: 1920, top: -200, right: 3200, bottom: 824 };
        let top_taskbar = RECT { left: 1920, top: -160, right: 3200, bottom: 824 };
        let normal = RECT { left: 2000, top: -180, right: 2600, bottom: 500 };
        assert_eq!(screen_rect(normal, secondary, top_taskbar), Some(RECT { left: 2000, top: -140, right: 2600, bottom: 540 }));
    }

    #[test]
    fn minimized_placement_uses_normal_rect_without_a_window() {
        let placement = WINDOWPLACEMENT { showCmd: 2, rcNormalPosition: RECT { left: 100, top: 100, right: 300, bottom: 300 }, ..Default::default() };
        let expected = unsafe { MonitorFromRect(&placement.rcNormalPosition, MONITOR_DEFAULTTONEAREST) };
        assert!(!expected.0.is_null());
        assert_eq!(placement_monitor(HWND(std::ptr::null_mut()), &placement), expected);
    }

    #[test]
    fn focus_guard_restores_only_the_refresh_that_stole_focus() {
        assert_eq!(focus_action(10, 10, false, false, true, false), FocusAction::Wait);
        assert_eq!(focus_action(10, 20, true, false, true, false), FocusAction::Restore);
        assert_eq!(focus_action(10, 20, true, true, true, false), FocusAction::Restore);
        assert_eq!(focus_action(10, 10, false, true, true, false), FocusAction::Done);
        assert_eq!(focus_action(10, 30, false, false, true, false), FocusAction::Done);
        assert_eq!(focus_action(10, 0, false, true, true, false), FocusAction::Wait);
        assert_eq!(focus_action(10, 0, false, false, false, false), FocusAction::Done);
        assert_eq!(focus_action(10, 0, false, false, true, true), FocusAction::Done);
        assert!(restoration_finished(false, false));
        assert!(!restoration_finished(false, true));
        assert!(restoration_finished(true, true));
    }
}
