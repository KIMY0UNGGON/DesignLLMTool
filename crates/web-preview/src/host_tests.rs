use super::*;
use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetWindow, PostMessageW, GA_ROOT, GW_OWNER};

#[test]
fn z_order_handover_requires_both_window_and_fallback_pairs() {
    assert_eq!(parse_z_order("42,7,43,8"), Some(((42, 7), (43, 8))));
    assert_eq!(parse_z_order("0,0,0,0"), Some(((0, 0), (0, 0))));
    for invalid in ["42,7", "42,7,43,8,9", "x,7,43,8", "42,-1,43,8"] {
        assert_eq!(parse_z_order(invalid), None);
    }
}

#[test]
fn anchored_preview_is_an_unowned_top_level_window() {
    let owner = create_host_window(None, true).unwrap();
    let preview = create_host_window(Some(owner), true).unwrap();
    unsafe {
        assert_eq!(GetAncestor(preview, GA_ROOT), preview);
        assert!(GetWindow(preview, GW_OWNER).is_err());
        assert!(!IsWindowVisible(preview).as_bool());
        let _ = DestroyWindow(preview);
        let _ = DestroyWindow(owner);
    }
}

#[test]
fn closing_does_not_hide_a_simultaneous_real_failure() {
    let result = finish_session(Err(DltError::new(ErrorCode::Unavailable, "memory limit")), true, false, SessionReport::default());
    assert_eq!(result.unwrap_err().message, "memory limit");
    assert!(finish_session(Err(DltError::new(ErrorCode::Canceled, "closed")), true, false, SessionReport::default()).is_ok());
    assert!(finish_session(Err(DltError::new(ErrorCode::Canceled, "cancelled")), true, true, SessionReport::default()).is_err());
}

#[test]
fn closing_during_startup_stops_the_wait_without_destroying_the_owner() {
    let owner = create_host_window(None, true).unwrap();
    let preview = create_host_window(Some(owner), true).unwrap();
    let (_sender, receiver) = mpsc::channel::<()>();
    assert!(!window_gone(preview, Some(owner)), "hidden does not mean closed");
    unsafe { PostMessageW(Some(preview), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap(); }
    assert!(matches!(pump_until(&receiver, Duration::from_secs(1), "startup", None, preview, Some(owner)), Err(HostError::Cancelled("startup"))));
    assert!(unsafe { IsWindow(Some(owner)).as_bool() });
    unsafe { let _ = DestroyWindow(owner); }
}

#[test]
fn destroyed_owner_ends_the_preview_session() {
    let owner = create_host_window(None, true).unwrap();
    let preview = create_host_window(Some(owner), true).unwrap();
    let (_sender, receiver) = mpsc::channel::<()>();
    unsafe { PostMessageW(Some(owner), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap(); }
    assert!(matches!(pump_until(&receiver, Duration::from_secs(1), "startup", None, preview, Some(owner)), Err(HostError::Cancelled("startup"))));
    assert!(window_gone(preview, Some(owner)));
    assert!(unsafe { IsWindow(Some(preview)).as_bool() }, "the lifetime anchor does not own the preview window");
    if unsafe { IsWindow(Some(preview)).as_bool() } { unsafe { let _ = DestroyWindow(preview); } }
}
