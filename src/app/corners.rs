//! Whether a window's corners can be left clear, for the design's rounded
//! window on Linux and Windows.
//!
//! Clew draws its borderless window there itself, frame and all; to round
//! its corners it leaves them clear, and what shows through is whatever
//! the window system composites under the window. Wayland always does. X
//! does while a compositing manager runs — and only for a window whose
//! pixels carry alpha (a 32-bit visual, which winit picks for a window
//! asked to be transparent): with neither, X shows a clear pixel as black,
//! and Clew squares its corners instead, as other toolkits do on such a
//! desktop.
//!
//! Windows composites what a window's swap chain holds opaque, whatever
//! its alpha; a DirectComposition visual's it composites by its alpha. So
//! where iced's GPU renderer can present through one (DX12), Clew's
//! windows do (`decide_presentation`), clear wherever Clew leaves them
//! so. Such a window is made transparent, which on Windows also makes it
//! without the surface GDI draws to (the vendored `iced_winit`), else shown
//! over its clear pixels; and Windows is asked to round none of its
//! corners and to draw no border of its own round it. Elsewhere Windows 11
//! rounds the window itself, to its own radius. Such a window shows nothing
//! iced draws in software: should iced fall back to software all the same,
//! Clew quits and starts again presenting directly
//! (`relaunch_presenting_directly`).

/// Whether Clew's windows present through a DirectComposition visual,
/// once [`decide_presentation`] has decided it.
#[cfg(windows)]
static THROUGH_VISUAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// What a relaunched Clew is told by, in its environment, to present its
/// windows directly (`relaunch_presenting_directly`).
#[cfg(windows)]
const PRESENT_DIRECTLY: &str = "CLEW_PRESENT_DIRECTLY";

/// Decide, before iced makes its renderer, whether Clew's windows present
/// through a DirectComposition visual, and if so ask iced's GPU renderer
/// to (the vendored `iced_wgpu`'s patch): where it draws first (iced's
/// `ICED_BACKEND` names no other) and gets, on DX12, the adapter and the
/// device it will ask for. Their corners are then the design's; otherwise
/// Windows rounds them. A window presenting so shows nothing iced draws in
/// software, so this asks for that adapter and device now, as the renderer
/// will (`visual_presentation_available`), rather than have iced fall back
/// to software with the window made for a visual. A Clew relaunched for
/// having found that happen anyway presents directly
/// ([`relaunch_presenting_directly`]). Called first in `main`, before any
/// thread is started.
#[cfg(windows)]
pub fn decide_presentation() {
    THROUGH_VISUAL.get_or_init(|| {
        let told = std::env::var_os(PRESENT_DIRECTLY).is_some();
        if told {
            // Taken out of the environment, for what Clew starts not to
            // inherit it (another Clew started from its terminal).
            // SAFETY: `main` decides this before it starts any thread, so
            // nothing reads or writes the environment meanwhile.
            unsafe { std::env::remove_var(PRESENT_DIRECTLY) };
        }
        let through = !told && visual_presentation_possible();
        if through {
            iced_wgpu::window::compositor::present_through_visual();
        }
        through
    });
}

/// Whether Clew's windows can present through a visual: iced's GPU
/// renderer draws first (`ICED_BACKEND` names no other) and gets its
/// adapter and device on DX12.
#[cfg(windows)]
fn visual_presentation_possible() -> bool {
    let gpu_first = std::env::var("ICED_BACKEND").map_or(true, |backends| {
        backends
            .split(',')
            .find(|backend| !backend.is_empty())
            .is_none_or(|first| first == "wgpu")
    });
    gpu_first && iced_wgpu::window::compositor::visual_presentation_available()
}

/// Start Clew again as it was started (its program and its arguments),
/// told to present its windows directly: as this one exits, having found
/// iced drawing in software a window it made for a visual, which shows
/// none of it (`shell::update`). Said on stderr if it cannot start.
#[cfg(windows)]
pub(crate) fn relaunch_presenting_directly() {
    if let Err(error) = relaunch_command().and_then(|mut command| command.spawn()) {
        eprintln!("clew: could not start again presenting directly: {error}");
    }
}

/// How [`relaunch_presenting_directly`] starts Clew.
#[cfg(windows)]
fn relaunch_command() -> std::io::Result<std::process::Command> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(std::env::args_os().skip(1))
        .env(PRESENT_DIRECTLY, "1");
    Ok(command)
}

/// Whether Clew's windows present through a visual, so their corners can
/// be left clear (`decide_presentation`): never off Windows, nor before
/// it is decided.
pub(crate) fn through_visual() -> bool {
    #[cfg(windows)]
    return THROUGH_VISUAL.get().copied().unwrap_or(false);
    #[cfg(not(windows))]
    false
}

/// Ask Windows to draw no border of its own round the window `window` (its
/// handle, as iced's raw id of it is): Windows 11 may draw one round a
/// window it does not round, over the corners Clew leaves clear. Windows
/// 10 draws none, and refuses the request.
#[cfg(windows)]
pub(crate) fn leave_edges_clear(window: u64) {
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DwmSetWindowAttribute,
    };
    let none: u32 = DWMWA_COLOR_NONE;
    // SAFETY: the attribute is the COLORREF (u32) it names, read during
    // the call; a handle no longer a window is refused, not used.
    let _ = unsafe {
        DwmSetWindowAttribute(
            window as windows_sys::Win32::Foundation::HWND,
            DWMWA_BORDER_COLOR as u32,
            (&raw const none).cast(),
            std::mem::size_of_val(&none) as u32,
        )
    };
}

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

/// Which pixels of a rounded corner show what is behind the window and
/// which show the window, as iced's GPU renderer draws a rounded quad: for
/// reading a window's corners off the screen (the Windows test above all).
#[cfg(test)]
mod profile {
    /// How much of the pixel at (`x`, `y`) of the top-left corner of a quad
    /// whose corners have `radius` (physical pixels) iced's quad shader
    /// covers: all of it whose centre is half a pixel or more inside the
    /// edge, none half a pixel or more outside, and in between in
    /// proportion (`clamp(0.5 - distance, 0, 1)`).
    pub(super) fn coverage(x: i32, y: i32, radius: f32) -> f32 {
        (0.5 - beyond(x, y, radius)).clamp(0.0, 1.0)
    }

    /// How far the centre of the pixel at (`x`, `y`) lies outside the edge
    /// of a corner of `radius` (negative: inside).
    fn beyond(x: i32, y: i32, radius: f32) -> f32 {
        let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
        let (dx, dy) = (radius - cx, radius - cy);
        if dx > 0.0 && dy > 0.0 {
            dx.hypot(dy) - radius
        } else {
            -cx.min(cy)
        }
    }

    /// The pixels of a top-left corner of `radius` that the shader leaves
    /// wholly clear (`false`) or wholly covers (`true`), each by a tenth of
    /// a pixel more than it must: what a screen shows of them is the colour
    /// behind or the colour drawn, never a blend of the two.
    pub(super) fn corner_profile(radius: f32) -> Vec<((i32, i32), bool)> {
        let side = radius.ceil() as i32;
        (0..side)
            .flat_map(|y| (0..side).map(move |x| (x, y)))
            .filter_map(|(x, y)| {
                let beyond = beyond(x, y, radius);
                if beyond >= 0.6 {
                    Some(((x, y), false))
                } else if beyond <= -0.6 {
                    Some(((x, y), true))
                } else {
                    None
                }
            })
            .collect()
    }

    /// The profile of the design's corner tells it from a corner a pixel
    /// smaller or larger, and from Windows 11's own: drawn so, some of its
    /// pixels would be covered a tenth or more differently, a difference a
    /// screen shows (a tenth of the test's colours apart is 12 levels or
    /// more in a channel). Radii nearer the design's than that the profile
    /// cannot tell from it.
    #[test]
    fn a_corner_profile_tells_its_radius_from_its_neighbours() {
        let radius = crate::ui::WINDOW_RADIUS;
        let profile = corner_profile(radius);
        let clear = profile.iter().filter(|&&(_, drawn)| !drawn).count();
        assert!(clear >= 20 && profile.len() - clear >= 80, "{profile:?}");
        let largest_difference = |other: f32| {
            profile
                .iter()
                .map(|&((x, y), drawn)| {
                    (coverage(x, y, other) - if drawn { 1.0 } else { 0.0 }).abs()
                })
                .fold(0.0_f32, f32::max)
        };
        assert_eq!(largest_difference(radius), 0.0);
        for other in [radius - 1.0, radius + 1.0, 8.0, 16.0] {
            assert!(
                largest_difference(other) >= 0.1,
                "a {other} px corner reads as the design's {radius}"
            );
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use iced::window::settings::platform::CornerPreference;
    use windows_sys::Win32::Foundation::HWND;

    /// A window that cannot present through a visual asks Windows to round
    /// its corners: iced's `corner_preference`, which winit hands to the
    /// compositor (DWM) as it makes the window. Windows 11 takes that of
    /// such a window — a popup with a sizing frame, as winit makes a
    /// borderless one; Windows 10 rounds no window, and is not asked here.
    #[test]
    fn windows_takes_the_request_to_round_the_window() {
        use windows_sys::Win32::Foundation::S_OK;
        use windows_sys::Win32::Graphics::Dwm::{
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_POPUP, WS_THICKFRAME,
        };

        assert_eq!(
            crate::window_settings_for(false)
                .platform_specific
                .corner_preference,
            CornerPreference::Round
        );
        let build = windows_build();
        if build < 22000 {
            // CI's Windows is of 11's era (`windows-latest`): passing there
            // by skipping would say nothing about the request being taken.
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "CI's Windows is build {build}, older than 11's: nothing would be tested",
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
        assert_eq!(answer, S_OK, "build {build}");
    }

    /// A relaunch starts this program again, with the arguments it was
    /// started with, told to present its windows directly.
    #[test]
    fn a_relaunch_starts_clew_as_it_was_started_presenting_directly() {
        let command = super::relaunch_command().expect("the program's path");
        assert_eq!(
            command.get_program(),
            std::env::current_exe().unwrap().as_os_str()
        );
        let started: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let given: Vec<&std::ffi::OsStr> = command.get_args().collect();
        assert_eq!(
            given,
            started.iter().map(|a| a.as_os_str()).collect::<Vec<_>>()
        );
        let told: Vec<_> = command.get_envs().collect();
        assert_eq!(
            told,
            [(
                std::ffi::OsStr::new(super::PRESENT_DIRECTLY),
                Some(std::ffi::OsStr::new("1"))
            )]
        );
    }

    /// The build of the Windows the tests run on.
    fn windows_build() -> u32 {
        use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
        use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
        let mut version = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        // SAFETY: `version` is an OSVERSIONINFOW with its size filled in.
        unsafe { RtlGetVersion(&mut version) };
        version.dwBuildNumber
    }

    /// The colour behind the window, as Windows reads it (0x00BBGGRR).
    const BEHIND: u32 = 0x0050_B41E;
    /// The colour the window is drawn in, as iced is given it and as the
    /// screen shows it.
    const DRAWN: (u8, u8, u8) = (0xD2, 0x3C, 0x8C);

    /// A window made as iced makes Clew's when it presents through a
    /// visual, drawn by iced's renderer as Clew draws its frame (filled to
    /// the design's rounded corners), shows on the screen what is under it
    /// at its corners and what it draws everywhere else: Windows neither
    /// rounds it nor draws a border round it, nor shows a surface of GDI's
    /// over it (the vendored `iced_winit` makes it without one), and
    /// composites it by its alpha. Read off the screen over a window of a
    /// known colour, at every pixel of each corner that the design's radius
    /// leaves wholly clear or wholly drawn (`profile`), which a radius a
    /// pixel smaller or larger, or Windows' own, would show otherwise.
    #[test]
    fn a_window_presented_through_a_visual_is_clear_at_its_corners() {
        use iced_winit::winit;
        use winit::platform::pump_events::EventLoopExtPumpEvents as _;
        use winit::platform::windows::EventLoopBuilderExtWindows as _;

        let on_ci = std::env::var_os("GITHUB_ACTIONS").is_some();
        if !super::visual_presentation_possible() {
            assert!(
                !on_ci,
                "CI's Windows has no DX12 adapter to present through a visual with: nothing would be tested",
            );
            return;
        }
        // Only compositors made from now on present so, and the only one
        // the tests make with a window is this test's.
        iced_wgpu::window::compositor::present_through_visual();
        let mut settings = crate::window_settings_for(true);
        assert!(settings.transparent);
        assert_eq!(
            settings.platform_specific.corner_preference,
            CornerPreference::DoNotRound
        );
        settings.size = iced::Size::new(360.0, 240.0);
        settings.min_size = None;
        settings.position = iced::window::Position::Specific(iced::Point::new(160.0, 140.0));
        settings.level = iced::window::Level::AlwaysOnTop;
        let attributes =
            iced_winit::conversion::window_attributes(settings, "Clew", 1.0, None, None);

        let mut event_loop = winit::event_loop::EventLoop::builder()
            .with_any_thread(true)
            .build()
            .expect("an event loop");
        let mut maker = Maker::default();
        let mut pump = |maker: &mut Maker, for_ms: u64| {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(for_ms);
            loop {
                let _ =
                    event_loop.pump_app_events(Some(std::time::Duration::from_millis(10)), maker);
                if std::time::Instant::now() >= until {
                    break;
                }
            }
        };

        maker.asked = Some(attributes);
        pump(&mut maker, 100);
        let window = maker.made.take().expect("the window is made");
        let (shown, report) = show_and_read(&window, &mut |for_ms| pump(&mut maker, for_ms));
        drop(window);
        pump(&mut maker, 100);
        assert!(
            shown,
            "the window as iced makes Clew's is not shown as drawn, with its corners clear: {report}"
        );
        // What was read, written past the harness's capture of a passing
        // test's output, for CI's log to show it.
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stdout(),
            "the window's corners are clear on the screen: {report}"
        );
    }

    /// Makes the window it is asked for, once winit lets it.
    #[derive(Default)]
    struct Maker {
        asked: Option<iced_winit::winit::window::WindowAttributes>,
        made: Option<std::sync::Arc<iced_winit::winit::window::Window>>,
    }

    impl Maker {
        fn make(&mut self, event_loop: &iced_winit::winit::event_loop::ActiveEventLoop) {
            if let Some(attributes) = self.asked.take() {
                let window = event_loop.create_window(attributes).expect("a window");
                self.made = Some(std::sync::Arc::new(window));
            }
        }
    }

    impl iced_winit::winit::application::ApplicationHandler for Maker {
        fn resumed(&mut self, event_loop: &iced_winit::winit::event_loop::ActiveEventLoop) {
            self.make(event_loop);
        }

        fn about_to_wait(&mut self, event_loop: &iced_winit::winit::event_loop::ActiveEventLoop) {
            self.make(event_loop);
        }

        fn window_event(
            &mut self,
            _: &iced_winit::winit::event_loop::ActiveEventLoop,
            _: iced_winit::winit::window::WindowId,
            _: iced_winit::winit::event::WindowEvent,
        ) {
        }
    }

    /// Put a window of the colour [`BEHIND`] under `window`, a little
    /// larger, draw `window` with iced's renderer as Clew draws its frame,
    /// and read the screen: whether it shows the corners clear and the
    /// rest drawn, and what it shows, said.
    fn show_and_read(
        window: &std::sync::Arc<iced_winit::winit::window::Window>,
        pump: &mut dyn FnMut(u64),
    ) -> (bool, String) {
        use iced::advanced::graphics::compositor::{self, Compositor as _};
        use iced::advanced::graphics::{Shell, Viewport};
        use iced::advanced::{Renderer as _, renderer};

        let hwnd = u64::from(window.id()) as HWND;
        super::leave_edges_clear(u64::from(window.id()));
        let size = window.inner_size();
        let origin = window.inner_position().expect("where the window is");
        let (x, y) = (origin.x, origin.y);
        let (w, h) = (size.width as i32, size.height as i32);
        let behind = Backdrop::new(hwnd, x - 24, y - 24, w + 48, h + 48);

        type Compositor = <iced::Renderer as compositor::Default>::Compositor;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        let made = runtime.block_on(Compositor::new(
            iced::advanced::graphics::Settings::default(),
            window.clone(),
            window.clone(),
            Shell::headless(),
        ));
        let mut compositor = match made {
            Ok(compositor) => compositor,
            Err(error) => return (false, format!("no renderer: {error}")),
        };
        let information = compositor.information();
        let drawn_by = format!("{} on {}", information.backend, information.adapter);
        let mut surface = compositor.create_surface(window.clone(), size.width, size.height);
        let viewport = Viewport::with_physical_size(
            iced::Size::new(size.width, size.height),
            window.scale_factor() as f32,
        );
        let (r, g, b) = DRAWN;
        let mut draw = || {
            let mut renderer = compositor.create_renderer();
            renderer.reset(iced::Rectangle::with_size(viewport.logical_size()));
            renderer.fill_quad(
                renderer::Quad {
                    bounds: iced::Rectangle::with_size(viewport.logical_size()),
                    border: iced::Border {
                        radius: crate::ui::WINDOW_RADIUS.into(),
                        ..iced::Border::default()
                    },
                    ..renderer::Quad::default()
                },
                iced::Color::from_rgb8(r, g, b),
            );
            compositor
                .present(
                    &mut renderer,
                    &mut surface,
                    &viewport,
                    iced::Color::TRANSPARENT,
                    || {},
                )
                .map_err(|error| format!("{error:?}"))
        };

        let drawn = u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16);
        let middle = (w / 2, h / 2);
        // What each pixel looked at should show: in each corner, every
        // pixel the design's radius leaves wholly clear or wholly drawn
        // (`super::profile`), which tells that radius from one a pixel
        // smaller or larger; each edge's middle and the window's, drawn (no
        // border of the system's); and round the window, what is behind it.
        let radius = crate::ui::WINDOW_RADIUS * window.scale_factor() as f32;
        let corners = super::profile::corner_profile(radius);
        let mut expected: Vec<((i32, i32), u32)> = Vec::new();
        for &((cx, cy), is_drawn) in &corners {
            let colour = if is_drawn { drawn } else { BEHIND };
            for at in [
                (cx, cy),
                (w - 1 - cx, cy),
                (cx, h - 1 - cy),
                (w - 1 - cx, h - 1 - cy),
            ] {
                expected.push((at, colour));
            }
        }
        for at in [
            middle,
            (w / 2, 0),
            (0, h / 2),
            (w - 1, h / 2),
            (w / 2, h - 1),
        ] {
            expected.push((at, drawn));
        }
        let around = [(-12, -12), (w + 11, h / 2)];
        expected.extend(around.iter().map(|&at| (at, BEHIND)));
        let ready: Vec<((i32, i32), u32)> = std::iter::once((middle, drawn))
            .chain(around.iter().map(|&at| (at, BEHIND)))
            .collect();
        let differing = |of: &[((i32, i32), u32)]| -> Vec<((i32, i32), u32, u32)> {
            let points: Vec<(i32, i32)> = of.iter().map(|&(at, _)| at).collect();
            of.iter()
                .zip(pixels(&points, x, y))
                .filter(|&(&(_, want), got)| !near(got, want))
                .map(|(&(at, want), got)| (at, want, got))
                .collect()
        };

        let mut wrong = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if let Err(error) = draw() {
                return (
                    false,
                    format!("drawn by {drawn_by}: presenting failed: {error}"),
                );
            }
            pump(60);
            if differing(&ready).is_empty() {
                // Drawn and on top: give the compositor a frame or two more.
                pump(250);
                let _ = draw();
                pump(250);
                wrong = Some(differing(&expected));
                break;
            }
        }
        // SAFETY: a point on the screen; no handle is kept.
        let on_top = unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::WindowFromPoint(
                windows_sys::Win32::Foundation::POINT {
                    x: x + middle.0,
                    y: y + middle.1,
                },
            )
        } == hwnd;
        drop(surface);
        drop(behind);
        let clear_count = corners.iter().filter(|&&(_, d)| !d).count() * 4;
        let drawn_count = corners.iter().filter(|&&(_, d)| d).count() * 4;
        let head = format!(
            "drawn by {drawn_by} at {x},{y} {w}x{h}, on top: {on_top}; behind {BEHIND:06X}, drawn {drawn:06X}; a {radius} px radius leaves {clear_count} corner pixels clear and {drawn_count} drawn"
        );
        match wrong {
            None => {
                let seen: Vec<String> = ready
                    .iter()
                    .map(|&(at, _)| at)
                    .zip(pixels(
                        &ready.iter().map(|&(at, _)| at).collect::<Vec<_>>(),
                        x,
                        y,
                    ))
                    .map(|((dx, dy), got)| format!("({dx},{dy})={got:06X}"))
                    .collect();
                (
                    false,
                    format!("{head}; never shown as drawn on top: {}", seen.join(" ")),
                )
            }
            Some(wrong) if wrong.is_empty() => (
                true,
                format!(
                    "{head}: all {} pixels read as expected (the corners, the edges' and the middle, and round the window)",
                    expected.len()
                ),
            ),
            Some(wrong) => {
                let listed: Vec<String> = wrong
                    .iter()
                    .take(48)
                    .map(|((dx, dy), want, got)| format!("({dx},{dy}) {got:06X} not {want:06X}"))
                    .collect();
                (
                    false,
                    format!(
                        "{head}; {} of {} pixels differ: {}",
                        wrong.len(),
                        expected.len(),
                        listed.join(", ")
                    ),
                )
            }
        }
    }

    /// Whether two colours Windows reads (0x00BBGGRR) are the same, to a
    /// rounding of a conversion or two.
    fn near(got: u32, want: u32) -> bool {
        (0..3).all(|channel| {
            let a = (got >> (8 * channel)) & 0xFF;
            let b = (want >> (8 * channel)) & 0xFF;
            a.abs_diff(b) <= 6
        })
    }

    /// The colours on the screen at `points`, each from (`x`, `y`), in
    /// physical pixels.
    fn pixels(points: &[(i32, i32)], x: i32, y: i32) -> Vec<u32> {
        use windows_sys::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC};
        // SAFETY: the screen's DC, released below; points are read from it.
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let colours = points
                .iter()
                .map(|&(dx, dy)| GetPixel(screen, x + dx, y + dy))
                .collect();
            ReleaseDC(std::ptr::null_mut(), screen);
            colours
        }
    }

    /// A plain window of the colour [`BEHIND`], kept under another.
    struct Backdrop {
        window: HWND,
        brush: windows_sys::Win32::Graphics::Gdi::HBRUSH,
        class: Vec<u16>,
    }

    impl Backdrop {
        fn new(over: HWND, x: i32, y: i32, w: i32, h: i32) -> Self {
            use windows_sys::Win32::Graphics::Dwm::{
                DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
            };
            use windows_sys::Win32::Graphics::Gdi::CreateSolidBrush;
            use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, RegisterClassW, SWP_NOACTIVATE, SWP_SHOWWINDOW,
                SetWindowPos, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_POPUP,
            };
            let class: Vec<u16> = "ClewTestBackdrop\0".encode_utf16().collect();
            // SAFETY: a class of this module, its procedure the system's
            // own; a top-level popup of it with no parent, menu or creation
            // data, destroyed on drop on this thread; the brush is the
            // class's until then.
            unsafe {
                let instance = GetModuleHandleW(std::ptr::null());
                let brush = CreateSolidBrush(BEHIND);
                let _ = RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(DefWindowProcW),
                    hInstance: instance,
                    hbrBackground: brush,
                    lpszClassName: class.as_ptr(),
                    ..Default::default()
                });
                let window = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    class.as_ptr(),
                    std::ptr::null(),
                    WS_POPUP,
                    x,
                    y,
                    w,
                    h,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null(),
                );
                assert!(!window.is_null(), "no window to put behind");
                let square = DWMWCP_DONOTROUND;
                let _ = DwmSetWindowAttribute(
                    window,
                    DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                    (&raw const square).cast(),
                    std::mem::size_of_val(&square) as u32,
                );
                super::leave_edges_clear(window as u64);
                // Right under the window, and shown.
                let _ = SetWindowPos(window, over, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
                Self {
                    window,
                    brush,
                    class,
                }
            }
        }
    }

    impl Drop for Backdrop {
        fn drop(&mut self) {
            use windows_sys::Win32::Graphics::Gdi::DeleteObject;
            use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
            use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyWindow, UnregisterClassW};
            // SAFETY: made in `new`, on this thread; the class has no
            // window left when it goes, nor the brush a class.
            unsafe {
                DestroyWindow(self.window);
                UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(std::ptr::null()));
                DeleteObject(self.brush);
            }
        }
    }
}
