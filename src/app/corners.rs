//! Whether a window's corners can be left clear, for the design's rounded
//! window on Linux.
//!
//! Clew draws its borderless window there itself, frame and all; to round
//! its corners it leaves them clear, and what shows through is whatever
//! the window system composites under the window. Wayland always does. X
//! does while a compositing manager runs — and only for a window whose
//! pixels carry alpha (a 32-bit visual, which winit picks for a window
//! asked to be transparent): with neither, X shows a clear pixel as black,
//! and Clew squares its corners instead, as other toolkits do on such a
//! desktop.

/// Whether X composites the X window `window` over the desktop: a
/// compositing manager holds `_NET_WM_CM_S<screen>` (the EWMH's own test),
/// and the window is 32 bits deep. `false` when X cannot be asked.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn x11_composited(window: u64) -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let Ok((conn, screen)) = x11rb::connect(None) else {
        return false;
    };
    let deep = u32::try_from(window)
        .ok()
        .and_then(|window| conn.get_geometry(window).ok()?.reply().ok())
        .is_some_and(|geometry| geometry.depth == 32);
    if !deep {
        return false;
    }
    // Asked only if it exists: an atom no manager ever named is no manager.
    let name = format!("_NET_WM_CM_S{screen}");
    let Some(atom) = conn
        .intern_atom(true, name.as_bytes())
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.atom)
        .filter(|&atom| atom != x11rb::NONE)
    else {
        return false;
    };
    conn.get_selection_owner(atom)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|reply| reply.owner != x11rb::NONE)
}

#[cfg(all(test, windows))]
mod tests {
    /// Clew asks Windows to round its borderless window's corners: iced's
    /// `corner_preference`, which winit hands to the compositor (DWM) as it
    /// makes the window. Windows 11 takes that of such a window — a popup
    /// with a sizing frame, as winit makes a borderless one; Windows 10
    /// rounds no window, and is not asked here.
    #[test]
    fn windows_takes_the_request_to_round_the_window() {
        use iced::window::settings::platform::CornerPreference;
        use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
        use windows_sys::Win32::Foundation::S_OK;
        use windows_sys::Win32::Graphics::Dwm::{
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
        };
        use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_POPUP, WS_THICKFRAME,
        };

        assert_eq!(
            crate::window_settings().platform_specific.corner_preference,
            CornerPreference::Round
        );
        let mut version = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        // SAFETY: `version` is an OSVERSIONINFOW with its size filled in.
        unsafe { RtlGetVersion(&mut version) };
        if version.dwBuildNumber < 22000 {
            // CI's Windows is of 11's era (`windows-latest`): passing there
            // by skipping would say nothing about the request being taken.
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI's Windows is build {}, older than 11's: nothing would be tested",
                version.dwBuildNumber
            );
            return;
        }
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: a top-level window of a system class, no parent, menu,
        // instance or creation data; destroyed below on this thread.
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                WS_POPUP | WS_THICKFRAME,
                0,
                0,
                320,
                200,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!window.is_null(), "no window to ask about");
        let round = DWMWCP_ROUND;
        // SAFETY: `window` is live; the attribute is the i32 it names.
        let answer = unsafe {
            DwmSetWindowAttribute(
                window,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                (&raw const round).cast(),
                std::mem::size_of_val(&round) as u32,
            )
        };
        // SAFETY: made above, on this thread.
        unsafe { DestroyWindow(window) };
        assert_eq!(answer, S_OK, "build {}", version.dwBuildNumber);
    }
}
