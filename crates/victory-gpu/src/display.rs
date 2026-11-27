//! LOCAL: how the game's window is shown: `VICTORY_WINDOW`, `VICTORY_WINDOW_SIZE`
//! and `VICTORY_MONITOR`.
//!
//! - `VICTORY_WINDOW=windowed` (a window with a frame, as before),
//!   `borderless` (a window without a frame covering the whole monitor) or
//!   `fullscreen` (the same borderless window: there is no exclusive mode,
//!   which would change the monitor's mode and lose the game on Alt+Tab).
//! - `VICTORY_WINDOW_SIZE=WIDTHxHEIGHT`: the client size of a framed window.
//! - `VICTORY_MONITOR=<n>`: the monitor, 1 for the first one Windows lists.
//!   Unset or past the last one: the primary monitor.
//!
//! With any of them set, Alt+Enter switches between a framed window and a
//! borderless one while the game runs, and the process is per-monitor DPI
//! aware (window sizes are real pixels on a scaled monitor). With none set nothing here runs and
//! the window is made exactly as before.

use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromWindow,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
use windows_sys::Win32::UI::WindowsAndMessaging as wm;

/// How the window is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Windowed,
    Borderless,
    Fullscreen,
}

impl Mode {
    /// Without a frame, covering the monitor.
    pub fn covers_monitor(self) -> bool {
        self != Mode::Windowed
    }
}

/// What the three variables ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    pub size: Option<(i32, i32)>,
    /// 1-based, in Windows' order.
    pub monitor: Option<usize>,
}

/// A rectangle in screen coordinates (a monitor left of the primary one
/// has negative ones).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Area {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Area {
    pub fn width(self) -> i32 {
        self.right - self.left
    }
    pub fn height(self) -> i32 {
        self.bottom - self.top
    }
    fn from_rect(r: RECT) -> Area {
        Area {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

/// One monitor: all of it, and the part the taskbar leaves free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub area: Area,
    pub work: Area,
    pub primary: bool,
}

/// Where a top-level window goes and with which styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub ex_style: u32,
    pub style: u32,
    pub rect: Area,
}

pub fn parse_mode(text: &str) -> Option<Mode> {
    match text.trim().to_ascii_lowercase().as_str() {
        "windowed" | "window" => Some(Mode::Windowed),
        "borderless" => Some(Mode::Borderless),
        "fullscreen" | "full" => Some(Mode::Fullscreen),
        _ => None,
    }
}

/// `WIDTHxHEIGHT`, both positive and at most 16384.
pub fn parse_size(text: &str) -> Option<(i32, i32)> {
    let (w, h) = text.trim().split_once(['x', 'X'])?;
    let (w, h): (i32, i32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    ((1..=16384).contains(&w) && (1..=16384).contains(&h)).then_some((w, h))
}

/// A monitor number, 1 or more.
pub fn parse_monitor(text: &str) -> Option<usize> {
    text.trim().parse().ok().filter(|&n| n >= 1)
}

/// The settings three variables' values ask for; `None` when all are unset.
/// A value that cannot be read is left out (and named in `bad`), so a typo
/// never stops the game.
pub fn settings_from(
    mode: Option<&str>,
    size: Option<&str>,
    monitor: Option<&str>,
    bad: &mut Vec<&'static str>,
) -> Option<Settings> {
    if mode.is_none() && size.is_none() && monitor.is_none() {
        return None;
    }
    let mut read = |name: &'static str, ok: bool| {
        if !ok {
            bad.push(name);
        }
    };
    let parsed_mode = mode.and_then(parse_mode);
    read("VICTORY_WINDOW", mode.is_none() || parsed_mode.is_some());
    let parsed_size = size.and_then(parse_size);
    read(
        "VICTORY_WINDOW_SIZE",
        size.is_none() || parsed_size.is_some(),
    );
    let parsed_monitor = monitor.and_then(parse_monitor);
    read(
        "VICTORY_MONITOR",
        monitor.is_none() || parsed_monitor.is_some(),
    );
    Some(Settings {
        mode: parsed_mode.unwrap_or(Mode::Windowed),
        size: parsed_size,
        monitor: parsed_monitor,
    })
}

/// The settings of this run, read once. `None`: the old window.
pub fn settings() -> Option<Settings> {
    static SETTINGS: OnceLock<Option<Settings>> = OnceLock::new();
    *SETTINGS.get_or_init(|| {
        let var = |name| std::env::var(name).ok();
        let (mode, size, monitor) = (
            var("VICTORY_WINDOW"),
            var("VICTORY_WINDOW_SIZE"),
            var("VICTORY_MONITOR"),
        );
        let mut bad = Vec::new();
        let settings = settings_from(mode.as_deref(), size.as_deref(), monitor.as_deref(), &mut bad);
        for name in bad {
            tracing::warn!(name, "ignoring an unreadable display setting");
        }
        if let Some(s) = settings {
            // The process is otherwise not DPI aware: on a scaled monitor
            // Windows reports a smaller monitor and stretches the window, so
            // a borderless window would be blurry. Per-monitor aware, sizes
            // are real pixels. Only with display settings; it fails, and
            // nothing changes, if the awareness was already set.
            // SAFETY: a process-wide setting, made before the first window.
            let aware = unsafe {
                windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                    windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                )
            } != 0;
            tracing::info!(mode = ?s.mode, size = ?s.size, monitor = ?s.monitor, aware, "display settings");
        }
        settings
    })
}

/// Monitor `n` (1-based), or the primary one when `n` is unset or past the
/// end, or the first one if none says it is primary.
pub fn pick(monitors: &[Monitor], n: Option<usize>) -> Option<Monitor> {
    n.and_then(|n| monitors.get(n - 1))
        .or_else(|| monitors.iter().find(|m| m.primary))
        .or_else(|| monitors.first())
        .copied()
}

/// A `width` x `height` rectangle centred in `work` (its top left kept
/// inside `work` when it is larger).
pub fn centred(work: Area, width: i32, height: i32) -> Area {
    let left = work.left + ((work.width() - width) / 2).max(0);
    let top = work.top + ((work.height() - height) / 2).max(0);
    Area {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// `width` x `height` scaled down to fit `work`, keeping its proportions,
/// or unchanged if it fits.
pub fn fit_into(work: Area, width: i32, height: i32) -> (i32, i32) {
    let (aw, ah) = (work.width(), work.height());
    if width <= 0 || height <= 0 || aw <= 0 || ah <= 0 || (width <= aw && height <= ah) {
        return (width, height);
    }
    let scale = f64::min(
        f64::from(aw) / f64::from(width),
        f64::from(ah) / f64::from(height),
    );
    (
        (f64::from(width) * scale) as i32,
        (f64::from(height) * scale) as i32,
    )
}

const FRAME: u32 =
    wm::WS_CAPTION | wm::WS_THICKFRAME | wm::WS_SYSMENU | wm::WS_MINIMIZEBOX | wm::WS_MAXIMIZEBOX;
const EX_FRAME: u32 =
    wm::WS_EX_WINDOWEDGE | wm::WS_EX_CLIENTEDGE | wm::WS_EX_DLGMODALFRAME | wm::WS_EX_STATICEDGE;

/// `style` without its frame: a popup.
pub fn popup_style(style: u32) -> u32 {
    (style & !(FRAME | wm::WS_MAXIMIZE | wm::WS_MINIMIZE)) | wm::WS_POPUP
}

/// `style` with a normal window frame.
pub fn framed_style(style: u32) -> u32 {
    (style & !wm::WS_POPUP) | wm::WS_OVERLAPPEDWINDOW
}

/// Where the window goes. `outer` is the size the window asked for, frame
/// included; `frame_of` turns a client size into the outer size of `style`;
/// `fit` shrinks an outer size into a work area.
pub fn place(
    settings: &Settings,
    monitor: &Monitor,
    ex_style: u32,
    style: u32,
    outer: (i32, i32),
    frame_of: impl Fn(u32, u32, i32, i32) -> (i32, i32),
    fit: impl Fn(Area, i32, i32) -> (i32, i32),
) -> Placement {
    if settings.mode.covers_monitor() {
        return Placement {
            ex_style: ex_style & !EX_FRAME,
            style: popup_style(style),
            rect: monitor.area,
        };
    }
    let (w, h) = match settings.size {
        Some((w, h)) => frame_of(ex_style, style, w, h),
        None => outer,
    };
    let (w, h) = fit(monitor.work, w, h);
    Placement {
        ex_style,
        style,
        rect: centred(monitor.work, w, h),
    }
}

/// The framed window Alt+Enter goes back to when the game started without
/// a frame: `VICTORY_WINDOW_SIZE`, or else three quarters of the work area
/// (client sizes; frame added by `frame_of`), centred.
pub fn framed_fallback(
    work: Area,
    size: Option<(i32, i32)>,
    style: u32,
    frame_of: impl Fn(u32, u32, i32, i32) -> (i32, i32),
) -> Area {
    let (cw, ch) = size.unwrap_or((work.width() * 3 / 4, work.height() * 3 / 4));
    let (w, h) = frame_of(0, style, cw, ch);
    let (w, h) = fit_into(work, w, h);
    centred(work, w, h)
}

/// The monitors, in Windows' order (the order the launcher lists them in).
pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn visit(monitor: HMONITOR, _: HDC, _: *mut RECT, found: LPARAM) -> i32 {
        // SAFETY: `found` is the vector `monitors` passed; the monitor
        // handle comes from the enumeration.
        unsafe {
            if let Some(m) = info(monitor) {
                (*(found as *mut Vec<Monitor>)).push(m);
            }
        }
        1
    }
    let mut found: Vec<Monitor> = Vec::new();
    // SAFETY: the callback only pushes to `found`, which outlives the call.
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(visit),
            &raw mut found as LPARAM,
        )
    };
    found
}

fn info(monitor: HMONITOR) -> Option<Monitor> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: a monitor handle and a structure of ours with its size set.
    (unsafe { GetMonitorInfoW(monitor, &mut info) } != 0).then(|| Monitor {
        area: Area::from_rect(info.rcMonitor),
        work: Area::from_rect(info.rcWork),
        primary: info.dwFlags & 1 != 0,
    })
}

/// The outer size of a window of `style` with a `w` x `h` client area.
pub fn frame_of(ex_style: u32, style: u32, w: i32, h: i32) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    // SAFETY: a plain computation on our rectangle.
    if unsafe { wm::AdjustWindowRectEx(&mut rect, style, 0, ex_style) } == 0 {
        return (w, h);
    }
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// Where a top-level window of `style` and outer size `outer` goes under
/// this run's settings; `None` when none are set (the old window).
pub fn place_now(
    ex_style: u32,
    style: u32,
    outer: (i32, i32),
    fit: impl Fn(Area, i32, i32) -> (i32, i32),
) -> Option<Placement> {
    let settings = settings()?;
    let monitor = pick(&monitors(), settings.monitor)?;
    let placed = place(&settings, &monitor, ex_style, style, outer, frame_of, fit);
    tracing::info!(?placed, ?monitor, "window placed for the display settings");
    Some(placed)
}

/// The framed window's styles and rectangle, kept while it is borderless.
static FRAMED: Mutex<Option<(isize, isize, Area)>> = Mutex::new(None);

/// Alt+Enter, seen as a system key: the Alt bit (29) of `lparam` set, not a
/// repeat (bit 30).
pub fn is_alt_enter(message: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    message == wm::WM_SYSKEYDOWN
        && wparam == usize::from(VK_RETURN)
        && lparam & (1 << 29) != 0
        && lparam & (1 << 30) == 0
}

/// Switches `hwnd` between a framed window and a borderless one covering
/// its monitor. Runs on the window's own thread, from its procedure.
/// `window` is the window's handle as a number.
pub fn toggle(window: isize) {
    let hwnd = window as *mut c_void;
    // SAFETY: a window of this process; styles and position only.
    unsafe {
        let style = wm::GetWindowLongPtrW(hwnd, wm::GWL_STYLE);
        let ex_style = wm::GetWindowLongPtrW(hwnd, wm::GWL_EXSTYLE);
        let Some(monitor) = info(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)) else {
            return;
        };
        let mut framed = FRAMED.lock().unwrap_or_else(|e| e.into_inner());
        let (new_style, new_ex, rect) = if (style as u32) & wm::WS_CAPTION == wm::WS_CAPTION {
            let mut now = RECT::default();
            wm::GetWindowRect(hwnd, &mut now);
            *framed = Some((style, ex_style, Area::from_rect(now)));
            (
                popup_style(style as u32) as isize,
                ((ex_style as u32) & !EX_FRAME) as isize,
                monitor.area,
            )
        } else {
            match framed.take() {
                Some(saved) => saved,
                None => {
                    let s = framed_style(style as u32);
                    let size = settings().and_then(|s| s.size);
                    (
                        s as isize,
                        ex_style,
                        framed_fallback(monitor.work, size, s, frame_of),
                    )
                }
            }
        };
        wm::SetWindowLongPtrW(hwnd, wm::GWL_STYLE, new_style);
        wm::SetWindowLongPtrW(hwnd, wm::GWL_EXSTYLE, new_ex);
        wm::SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            rect.left,
            rect.top,
            rect.width(),
            rect.height(),
            wm::SWP_FRAMECHANGED | wm::SWP_NOZORDER | wm::SWP_NOOWNERZORDER | wm::SWP_SHOWWINDOW,
        );
        tracing::info!(
            borderless = (new_style as u32) & wm::WS_POPUP != 0,
            ?rect,
            "Alt+Enter"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(left: i32, top: i32, right: i32, bottom: i32) -> Area {
        Area {
            left,
            top,
            right,
            bottom,
        }
    }

    /// A 2560x1440 primary monitor with a 48 px taskbar, and a 1920x1080
    /// one to its left.
    fn layout() -> [Monitor; 2] {
        [
            Monitor {
                area: area(0, 0, 2560, 1440),
                work: area(0, 0, 2560, 1392),
                primary: true,
            },
            Monitor {
                area: area(-1920, 360, 0, 1440),
                work: area(-1920, 360, 0, 1400),
                primary: false,
            },
        ]
    }

    /// A frame of 8 px each side and a 31 px title bar.
    fn frame(_: u32, _: u32, w: i32, h: i32) -> (i32, i32) {
        (w + 16, h + 39)
    }

    fn windowed(size: Option<(i32, i32)>, monitor: Option<usize>) -> Settings {
        Settings {
            mode: Mode::Windowed,
            size,
            monitor,
        }
    }

    #[test]
    fn modes_sizes_and_monitors_parse() {
        assert_eq!(parse_mode("windowed"), Some(Mode::Windowed));
        assert_eq!(parse_mode(" Borderless "), Some(Mode::Borderless));
        assert_eq!(parse_mode("FULLSCREEN"), Some(Mode::Fullscreen));
        assert_eq!(parse_mode("exclusive"), None);
        assert_eq!(parse_size("1600x900"), Some((1600, 900)));
        assert_eq!(parse_size(" 1280 X 720 "), Some((1280, 720)));
        for bad in [
            "",
            "1600",
            "0x900",
            "1600x0",
            "-1x2",
            "1x2x3",
            "99999x900",
            "axb",
        ] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
        assert_eq!(parse_monitor("2"), Some(2));
        assert_eq!(parse_monitor("0"), None);
        assert_eq!(parse_monitor("two"), None);
    }

    #[test]
    fn nothing_set_means_the_old_window() {
        let mut bad = Vec::new();
        assert_eq!(settings_from(None, None, None, &mut bad), None);
        assert!(bad.is_empty());
    }

    #[test]
    fn unreadable_values_are_named_and_left_out() {
        let mut bad = Vec::new();
        let s = settings_from(Some("huge"), Some("big"), Some("2"), &mut bad).unwrap();
        assert_eq!(s, windowed(None, Some(2)));
        assert_eq!(bad, ["VICTORY_WINDOW", "VICTORY_WINDOW_SIZE"]);
    }

    #[test]
    fn the_monitor_is_numbered_from_one_and_falls_back_to_the_primary() {
        let monitors = layout();
        assert_eq!(pick(&monitors, Some(2)), Some(monitors[1]));
        assert_eq!(pick(&monitors, Some(1)), Some(monitors[0]));
        assert_eq!(pick(&monitors, Some(3)), Some(monitors[0]));
        assert_eq!(pick(&monitors, None), Some(monitors[0]));
        let no_primary = [Monitor {
            primary: false,
            ..monitors[1]
        }];
        assert_eq!(pick(&no_primary, None), Some(no_primary[0]));
        assert_eq!(pick(&[], None), None);
    }

    #[test]
    fn borderless_and_fullscreen_cover_the_whole_monitor_without_a_frame() {
        let monitors = layout();
        for mode in [Mode::Borderless, Mode::Fullscreen] {
            let s = Settings {
                mode,
                size: Some((800, 600)),
                monitor: Some(2),
            };
            let placed = place(
                &s,
                &monitors[1],
                wm::WS_EX_WINDOWEDGE | wm::WS_EX_APPWINDOW,
                wm::WS_OVERLAPPEDWINDOW | wm::WS_VISIBLE,
                (3840, 2160),
                frame,
                fit_into,
            );
            assert_eq!(placed.rect, area(-1920, 360, 0, 1440));
            assert_eq!(placed.style, wm::WS_POPUP | wm::WS_VISIBLE);
            assert_eq!(placed.ex_style, wm::WS_EX_APPWINDOW);
        }
    }

    #[test]
    fn a_window_size_is_the_client_size_centred_in_the_work_area() {
        let monitors = layout();
        let style = wm::WS_OVERLAPPEDWINDOW | wm::WS_VISIBLE;
        let placed = place(
            &windowed(Some((1600, 900)), None),
            &monitors[0],
            0,
            style,
            (3840, 2160),
            frame,
            fit_into,
        );
        assert_eq!(placed.style, style);
        assert_eq!(placed.rect, area(472, 226, 2088, 1165));
        // On the second monitor, left of the primary.
        let placed = place(
            &windowed(Some((1280, 720)), Some(2)),
            &monitors[1],
            0,
            style,
            (3840, 2160),
            frame,
            fit_into,
        );
        assert_eq!(placed.rect, area(-1608, 500, -312, 1259));
    }

    #[test]
    fn a_window_larger_than_the_work_area_shrinks_to_it() {
        let monitors = layout();
        // No size: the title's 3840x2160, fitted, on monitor 2.
        let placed = place(
            &windowed(None, Some(2)),
            &monitors[1],
            0,
            0,
            (3840, 2160),
            frame,
            fit_into,
        );
        assert_eq!(placed.rect, area(-1884, 360, -36, 1400));
        let placed = place(
            &windowed(Some((3840, 2160)), None),
            &monitors[0],
            0,
            0,
            (0, 0),
            frame,
            fit_into,
        );
        assert_eq!(placed.rect.width(), 2440);
        assert!((1391..=1392).contains(&placed.rect.height()));
        assert_eq!((placed.rect.left, placed.rect.top), (60, 0));
    }

    #[test]
    fn alt_enter_goes_back_to_a_framed_window_that_fits() {
        let work = layout()[0].work;
        assert_eq!(
            framed_fallback(work, None, 0, frame),
            area(312, 154, 2248, 1237)
        );
        assert_eq!(
            framed_fallback(work, Some((1280, 720)), 0, frame),
            area(632, 316, 1928, 1075)
        );
    }

    #[test]
    fn styles_lose_and_gain_the_frame() {
        let framed = wm::WS_OVERLAPPEDWINDOW | wm::WS_VISIBLE | wm::WS_MAXIMIZE;
        assert_eq!(popup_style(framed), wm::WS_POPUP | wm::WS_VISIBLE);
        assert_eq!(
            framed_style(wm::WS_POPUP | wm::WS_VISIBLE),
            wm::WS_OVERLAPPEDWINDOW | wm::WS_VISIBLE
        );
    }

    #[test]
    fn only_a_fresh_alt_enter_switches() {
        let alt = 1 << 29;
        assert!(is_alt_enter(wm::WM_SYSKEYDOWN, VK_RETURN.into(), alt));
        assert!(!is_alt_enter(
            wm::WM_SYSKEYDOWN,
            VK_RETURN.into(),
            alt | (1 << 30)
        ));
        assert!(!is_alt_enter(wm::WM_KEYDOWN, VK_RETURN.into(), alt));
        assert!(!is_alt_enter(wm::WM_SYSKEYDOWN, VK_RETURN.into(), 0));
        assert!(!is_alt_enter(wm::WM_SYSKEYDOWN, 0x73, alt));
    }
}
